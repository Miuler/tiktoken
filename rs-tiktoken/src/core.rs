// This check is new and seems buggy (possibly with PyO3 interaction)
#![allow(clippy::borrow_deref_ref)]

use std::collections::HashSet;
use std::num::NonZeroU64;
use std::thread;

use fancy_regex::Regex;
use rustc_hash::FxHashMap as HashMap;

use crate::error::TiktokenError;

fn _byte_pair_merge<T>(
    piece: &[u8],
    ranks: &HashMap<Vec<u8>, usize>,
    f: impl Fn(std::ops::Range<usize>) -> T,
) -> Vec<T> {
    // This is a vector of (start, rank).
    // The rank is of the byte pair starting at position start.
    // The rank of the last item in the vector is not a valid value.
    let mut parts: Vec<(usize, usize)> = (0..piece.len() + 1).map(|i| (i, usize::MAX)).collect();

    let get_rank = {
        #[inline(always)]
        |parts: &Vec<(usize, usize)>, start_idx: usize, skip: usize| {
            if (start_idx + skip + 2) < parts.len() {
                ranks
                    .get(&piece[parts[start_idx].0..parts[start_idx + skip + 2].0])
                    .copied()
            } else {
                None
            }
        }
    };

    // We look up the ranks once in the beginning and iteratively update
    // them during each merge, which reduces the number of rank lookups.
    for i in 0..parts.len() - 2 {
        match get_rank(&parts, i, 0) {
            Some(rank) => {
                // usize::MAX is a sentinel value and cannot be a valid rank
                debug_assert!(rank != usize::MAX);
                parts[i].1 = rank;
            }
            None => {
                continue;
            }
        };
    }

    // If you have n parts and m merges, this does O(mn) work.
    // We could do something with a heap and do O(m log n) work.
    // It is important to consider that n is often small (<100), and as such
    // the cache-locality benefits outweigh the algorithmic complexity downsides
    // of the `parts` vector data structure above.

    // Note that we hash bytes, not token pairs. As long as we train BPE the way we
    // currently do, this is equivalent. An easy way to break this would be to decouple
    // merge priority from token index or to prevent specific token merges.
    loop {
        if parts.len() == 1 {
            break;
        }

        // usize::MAX is a sentinel rank value allowing us to
        // take the min more quickly
        let mut min_rank: (usize, usize) = (usize::MAX, 0);
        for (i, &(_, rank)) in parts[..parts.len() - 1].iter().enumerate() {
            if rank < min_rank.0 {
                min_rank = (rank, i);
            }
        }

        if min_rank.0 != usize::MAX {
            let i = min_rank.1;

            // NOTE: We are about to remove parts[i + 1]. We do not do it
            // yet because there are cache-locality benefits to updating
            // parts[i] and parts[i-1] before removing, which could thrash
            // the cache. Thus, we update the rank calculation by skipping over
            // parts[i + 1], by invoking `get_rank!` with `skip = 1`.
            parts[i].1 = get_rank(&parts, i, 1).unwrap_or(usize::MAX);
            if i > 0 {
                parts[i - 1].1 = get_rank(&parts, i - 1, 1).unwrap_or(usize::MAX);
            }

            parts.remove(i + 1);
        } else {
            break;
        }
    }
    let mut out: Vec<T> = Vec::with_capacity(parts.len() - 1);
    for i in 0..parts.len() - 1 {
        out.push(f(parts[i].0..parts[i + 1].0));
    }
    out
}

pub fn byte_pair_encode(piece: &[u8], ranks: &HashMap<Vec<u8>, usize>) -> Vec<usize> {
    if piece.len() == 1 {
        return vec![ranks[piece]];
    }
    _byte_pair_merge(piece, ranks, |p| ranks[&piece[p.start..p.end]])
}

pub fn byte_pair_split<'a>(piece: &'a [u8], ranks: &HashMap<Vec<u8>, usize>) -> Vec<&'a [u8]> {
    if piece.len() == 1 {
        return vec![piece];
    }
    _byte_pair_merge(piece, ranks, |p| &piece[p.start..p.end])
}

// Various performance notes:
//
// Regex
// =====
// Most of the time is spent in regex. The easiest way to speed this up is by using less fancy
// regex features. For instance, using a regex parse-able by `regex` crate is 3x faster than
// the usual regex we use.
//
// However, given that we're using a regex parse-able by `regex`, there isn't much difference
// between using the `regex` crate and using the `fancy_regex` crate.
//
// There is an important interaction between threading, `regex` and `fancy_regex`.
// When using `fancy_regex`, we hit `regex.find_at`. It turns out that this causes contention on
// some mutable scratch space inside of `regex`. This absolutely kills performance. When using plain
// old `regex`, we don't hit this, because `find_iter` has a different code path.
// Related: https://github.com/rust-lang/regex/blob/master/PERFORMANCE.md
// Anyway, the way we get around this is with having a (mostly) thread local clone of the regex for
// each thread.
//
// Threading
// =========
// I tried using `rayon`. It wasn't really faster than using Python threads and releasing the GIL.
// So goodbye `rayon`! Let thread count etc be in control of our Python users.
//
// Caching
// =======
// The reference tokeniser has an lru cache over the equivalent of `byte_pair_encode`.
// Originally, we had one too! Without it, we were only vaguely faster than Python.
// I used an RWLock to protect the cache. This didn't seem to hurt single threaded performance
// noticeably, but it did affect multi-threaded performance. Weirdly, it seemed to affect
// multi-threaded performance even when I only had readers (maybed I messed something up?).
// Anyway, I realised that we could get rid of the cache, if we treat the set of tokens as a cache!
// These are exactly the set or merges that are likely to be hot. And now we don't have to think
// about interior mutability, memory use, or cloning.
//
// Hashing
// =======
// We use FxHashMap instead of the standard HashMap. This is maybe like a 5-10% win?
// The current implementation ends up doing a lot of hashing of bytes. In theory, this could be made
// to be hashing of two-tuples of ints, which looks like it may also be a couple percent faster.

pub struct FakeThreadId(NonZeroU64);

fn hash_current_thread() -> usize {
    // It's easier to use unsafe than to use nightly. Rust has this nice u64 thread id counter
    // that works great for our use case of avoiding collisions in our array. Unfortunately,
    // it's private. However, there are only so many ways you can layout a u64, so just transmute
    // https://github.com/rust-lang/rust/issues/67939
    const _: [u8; 8] = [0; std::mem::size_of::<std::thread::ThreadId>()];
    const _: [u8; 8] = [0; std::mem::size_of::<FakeThreadId>()];
    let x = unsafe {
        std::mem::transmute::<std::thread::ThreadId, FakeThreadId>(thread::current().id()).0
    };
    u64::from(x) as usize
}

pub const MAX_NUM_THREADS: usize = 128;

pub struct CoreBPE {
    pub encoder: HashMap<Vec<u8>, usize>,
    pub special_tokens_encoder: HashMap<String, usize>,
    pub decoder: HashMap<usize, Vec<u8>>,
    pub special_tokens_decoder: HashMap<usize, Vec<u8>>,
    pub regex_tls: Vec<Regex>,
    pub special_regex_tls: Vec<Regex>,
    pub sorted_token_bytes: Vec<Vec<u8>>,
}

impl CoreBPE {
    /// Create a new BPE model.
    ///
    /// * `encoder` - A mapping from byte sequences to their rank (python field Encoding.mergeable_ranks).
    /// * `special_tokens_encoder` - A mapping from special tokens to their rank (python field Encoding.special_tokens).
    /// * `pattern` - A regex pattern that matches all tokens (python field Encoding.pat_str).
    pub fn new(
        encoder: HashMap<Vec<u8>, usize>,
        special_tokens_encoder: HashMap<String, usize>,
        pattern: &str,
    ) -> Result<Self, TiktokenError> {
        let regex = Regex::new(pattern)?;

        let special_regex = {
            let _parts = special_tokens_encoder
                .keys()
                .map(|s| fancy_regex::escape(s))
                .collect::<Vec<_>>();
            Regex::new(&_parts.join("|"))?
        };

        let decoder: HashMap<usize, Vec<u8>> =
            encoder.iter().map(|(k, v)| (*v, k.clone())).collect();

        assert!(
            encoder.len() == decoder.len(),
            "Encoder and decoder must be of equal length; maybe you had duplicate token indices in your encoder?"
        );

        let special_tokens_decoder: HashMap<usize, Vec<u8>> = special_tokens_encoder
            .iter()
            .map(|(k, v)| (*v, k.as_bytes().to_vec()))
            .collect();

        // Clone because I don't know how to tell Rust I'm not going to change the map
        let mut sorted_token_bytes: Vec<Vec<u8>> = encoder.keys().cloned().collect();
        sorted_token_bytes.sort();

        let core_bpe = CoreBPE {
            encoder,
            special_tokens_encoder,
            decoder,
            special_tokens_decoder,
            regex_tls: (0..MAX_NUM_THREADS).map(|_| regex.clone()).collect(),
            special_regex_tls: (0..MAX_NUM_THREADS)
                .map(|_| special_regex.clone())
                .collect(),
            sorted_token_bytes,
        };
        Ok(core_bpe)
    }

    pub fn _get_tl_regex(&self) -> &Regex {
        // See performance notes above for what this is about
        // It's also a little janky, please make a better version of it!
        // However, it's nice that this doesn't leak memory to short-lived threads
        &self.regex_tls[hash_current_thread() % MAX_NUM_THREADS]
    }

    pub fn _get_tl_special_regex(&self) -> &Regex {
        &self.special_regex_tls[hash_current_thread() % MAX_NUM_THREADS]
    }

    pub fn _decode_native(&self, tokens: &[usize]) -> Vec<u8> {
        let mut ret = Vec::with_capacity(tokens.len() * 2);
        for token in tokens {
            let token_bytes = self
                .decoder
                .get(token)
                .unwrap_or_else(|| &self.special_tokens_decoder[token]);
            ret.extend(token_bytes);
        }
        ret
    }

    pub fn _encode_ordinary_native(&self, text: &str) -> Vec<usize> {
        // This is the core of the encoding logic; the other functions in here
        // just make things complicated :-)
        let regex = self._get_tl_regex();
        let mut ret = vec![];
        for mat in regex.find_iter(text) {
            let piece = mat.unwrap().as_str().as_bytes();
            if let Some(token) = self.encoder.get(piece) {
                ret.push(*token);
                continue;
            }
            ret.extend(&byte_pair_encode(piece, &self.encoder));
        }
        ret
    }

    pub fn _encode_bytes(&self, bytes: &[u8]) -> Vec<usize> {
        match std::str::from_utf8(bytes) {
            Ok(text) => self._encode_ordinary_native(text),
            Err(e) => {
                let text = unsafe { std::str::from_utf8_unchecked(&bytes[..e.valid_up_to()]) };
                let (tokens, last_piece_token_len) = self._encode_native(text, &HashSet::new());
                let (mut tokens, last_piece_token_len) =
                    self._increase_last_piece_token_len(tokens, last_piece_token_len);
                if !tokens.is_empty() && last_piece_token_len > 0 {
                    // Lop off the tokens from the last piece and run BPE on the remaining bytes
                    // Somewhat niche, but this may not be correct if we'd have had a regex
                    // split between the valid UTF-8 and the invalid bytes, which is why this
                    // method is private
                    let mut unstable_bytes =
                        self._decode_native(&tokens[tokens.len() - last_piece_token_len..]);
                    unstable_bytes.extend_from_slice(&bytes[e.valid_up_to()..]);

                    tokens.truncate(tokens.len() - last_piece_token_len);
                    tokens.extend(byte_pair_encode(&unstable_bytes, &self.encoder));
                }
                tokens
            }
        }
    }

    pub fn _encode_native(&self, text: &str, allowed_special: &HashSet<&str>) -> (Vec<usize>, usize) {
        let special_regex = self._get_tl_special_regex();
        let regex = self._get_tl_regex();
        let mut ret = vec![];

        let mut start = 0;
        let mut last_piece_token_len = 0;
        loop {
            let mut next_special;
            let mut start_find = start;
            loop {
                // Find the next allowed special token, if any
                next_special = special_regex.find_from_pos(text, start_find).unwrap();
                match next_special {
                    Some(m) => {
                        if allowed_special.contains(&text[m.start()..m.end()]) {
                            break;
                        }
                        start_find = m.start() + 1;
                    }
                    None => break,
                }
            }
            let end = next_special.map_or(text.len(), |m| m.start());

            // Okay, here we go, compare this logic to _encode_ordinary_native
            for mat in regex.find_iter(&text[start..end]) {
                let piece = mat.unwrap().as_str().as_bytes();
                if let Some(token) = self.encoder.get(piece) {
                    last_piece_token_len = 1;
                    ret.push(*token);
                    continue;
                }
                let tokens = byte_pair_encode(piece, &self.encoder);
                last_piece_token_len = tokens.len();
                ret.extend(&tokens);
            }

            match next_special {
                // And here we push the special token
                Some(m) => {
                    let piece = m.as_str();
                    let token = self.special_tokens_encoder[piece];
                    ret.push(token);
                    start = m.end();
                    last_piece_token_len = 0;
                }
                None => break,
            }
        }

        // last_piece_token_len is how many tokens came from the last regex split. This is used
        // for determining unstable tokens, since you can't merge across (stable) regex splits
        (ret, last_piece_token_len)
    }

    pub fn _increase_last_piece_token_len(
        &self,
        tokens: Vec<usize>,
        mut last_piece_token_len: usize,
    ) -> (Vec<usize>, usize) {
        // Unfortunately, the locations where our regex splits can be unstable.
        // For the purposes of determining unstable tokens, unstable regex splitting
        // is only a problem if a split that was present disappears, since this can
        // lead to merging of tokens otherwise thought to be stable.
        // cl100k_base makes our life hard by including the \s*[\r\n]+
        // pattern. This can e.g. cause "\n" + " " to become "\n \n".
        // Here is a quick and dirty fix:
        {
            let token_is_all_space = |token| {
                self.decoder
                    .get(token)
                    .map(|token_bytes| {
                        token_bytes
                            .iter()
                            .rev()
                            .all(|&b| [b' ', b'\n', b'\t'].contains(&b))
                    })
                    .unwrap_or(false)
            };
            if last_piece_token_len > 0
                && token_is_all_space(&tokens[tokens.len() - last_piece_token_len])
            {
                while (last_piece_token_len < tokens.len())
                    && token_is_all_space(&tokens[tokens.len() - last_piece_token_len - 1])
                {
                    last_piece_token_len += 1;
                }
            }
        }
        debug_assert!(last_piece_token_len <= tokens.len());

        (tokens, last_piece_token_len)
    }

    pub fn _encode_unstable_native(
        &self,
        text: &str,
        allowed_special: &HashSet<&str>,
    ) -> (Vec<usize>, HashSet<Vec<usize>>) {
        let (tokens, last_piece_token_len) = self._encode_native(text, allowed_special);
        if last_piece_token_len == 0 {
            // If last_piece_token_len is zero, the last token was a special token and we have
            // no unstable bytes
            return (tokens, HashSet::new());
        }
        let (mut tokens, last_piece_token_len) =
            self._increase_last_piece_token_len(tokens, last_piece_token_len);

        let unstable_bytes = self._decode_native(&tokens[tokens.len() - last_piece_token_len..]);
        tokens.truncate(tokens.len() - last_piece_token_len);

        // TODO: we should try harder to find additional stable tokens
        // This would reduce the amount of retokenising when determining completions
        // Refer to the logic in an older version of this file

        let mut completions = HashSet::new();
        if unstable_bytes.is_empty() {
            return (tokens, completions);
        }

        // This is the easy bit. Just find all single tokens that start with unstable_bytes
        // (including tokens that exactly match unstable_bytes)
        // Separating this from the loop below helps with performance in a common case.
        let mut point = self
            .sorted_token_bytes
            .partition_point(|x| x.as_slice() < unstable_bytes.as_slice());
        while point < self.sorted_token_bytes.len()
            && self.sorted_token_bytes[point].starts_with(&unstable_bytes)
        {
            completions.insert(vec![
                self.encoder[self.sorted_token_bytes[point].as_slice()],
            ]);
            point += 1;
        }

        // Now apply even more brute force. At every (other) possible position for the straddling
        // token, concatenate additional bytes from that token (if any) to unstable_bytes,
        // and retokenise the whole thing and see what we get.
        for i in 1..unstable_bytes.len() {
            let prefix = &unstable_bytes[..i];
            let suffix = &unstable_bytes[i..];
            let mut point = self
                .sorted_token_bytes
                .partition_point(|x| x.as_slice() < suffix);
            // TODO: Perf optimisation if suffix starts with " "?
            while point < self.sorted_token_bytes.len()
                && self.sorted_token_bytes[point].starts_with(suffix)
            {
                let possibility = [prefix, self.sorted_token_bytes[point].as_slice()].concat();
                let encoded = match std::str::from_utf8(&possibility) {
                    // Morally, this is byte_pair_encode(&possibility, &self.encoder)
                    // But we might have introduced a regex split which would prevent merges.
                    // (particularly possible in the presence of unstable regex splits)
                    // So convert to UTF-8 and do regex splitting.
                    // E.g. with cl100k_base "  !" gets split to " " + " !",
                    // but byte_pair_encode("  !") != byte_pair_encode(" ")
                    Ok(s) => self._encode_ordinary_native(s),

                    // Technically, whether or not this arm is correct depends on whether there
                    // would be a regex split before the UTF-8 truncation point.
                    // Probably niche enough that no one will ever notice (after all, people didn't
                    // notice all the big holes in the previous unstable token implementation)
                    Err(_) => byte_pair_encode(&possibility, &self.encoder),
                    // Something like the following is intriguing but incorrect:
                    // Err(e) => self._encode_ordinary_native(unsafe {
                    //     std::str::from_utf8_unchecked(&possibility[..e.valid_up_to()])
                    // }),
                };
                let mut seq = Vec::new();
                let mut seq_len = 0;
                for token in encoded {
                    seq.push(token);
                    seq_len += self.decoder[&token].len();
                    if seq_len >= unstable_bytes.len() {
                        break;
                    }
                }
                completions.insert(seq);
                point += 1;
            }
        }

        // This is also not straightforward. While we generally assume that regex splits are stable,
        // unfortunately, they are not. That is, if adding bytes were to make a split appear in
        // unstable_bytes, this could make tokens possible which our logic would otherwise think
        // would be merged.
        // For example, with gpt2, the use of \s+(?!\S) means that "\n\n" could
        // develop a split, e.g. "\n\n0" splits into "\n"+"\n"+"0", making "\n" a possible token.
        // Here is a quick and dirty fix:
        // This isn't right if we ever remove \s+(?!\S)
        if unstable_bytes.len() > 1 {
            let last_decoded = bstr::decode_last_utf8(unstable_bytes.as_slice());
            if unstable_bytes.len() - last_decoded.1 > 0
                && last_decoded.0.map_or(false, |c| c.is_whitespace())
            {
                let mut reencoded = byte_pair_encode(
                    &unstable_bytes[..unstable_bytes.len() - last_decoded.1],
                    &self.encoder,
                );
                reencoded.extend(byte_pair_encode(
                    &unstable_bytes[unstable_bytes.len() - last_decoded.1..],
                    &self.encoder,
                ));
                completions.insert(reencoded);
            }
        }

        (tokens, completions)
    }
}


#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use once_cell::sync::Lazy;
    use super::*;

    static GPT2_CORE_BPE: Lazy<CoreBPE> = Lazy::new(|| {
        println!("Loading GPT2 BPE");

        let mut encoder: HashMap<Vec<u8>, usize> = Default::default();
        let file = File::open("tests/gpt2_encoder").unwrap();
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line.unwrap();
            let line = line.split(": ").collect::<Vec<&str>>();

            let key = line[0];
            let key = &key[1..key.len()-1];
            let key = key
                .split(", ")
                .map(|n| n.parse::<u8>().unwrap())
                .collect::<Vec<u8>>();

            let value = line[1].parse::<usize>().unwrap();
            encoder.insert(key, value);
        };

        let mut special_tokens_encoder: HashMap<String, usize> = Default::default();
        special_tokens_encoder.insert("<|endoftext|>".to_string(), 50256);

        let pattern = "'s|'t|'re|'ve|'m|'ll|'d| ?\\p{L}+| ?\\p{N}+| ?[^\\s\\p{L}\\p{N}]+|\\s+(?!\\S)|\\s+";

        CoreBPE::new(
            encoder,
            special_tokens_encoder,
            pattern,
        ).unwrap()
    });


    #[test]
    fn test_simple_gpt2() {
        let (tokens, _last_piece_token_len) = GPT2_CORE_BPE._encode_native("hello world", &Default::default());

        assert_eq!(tokens, vec![31373, 995]);
    }

    #[test]
    fn test_simple_repeated_gpt2() {

        let allowed_special: &HashSet<&str> = &Default::default();

        assert_eq!(GPT2_CORE_BPE._encode_native("0", allowed_special).0, vec![15]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00", allowed_special).0, vec![405]);
        assert_eq!(GPT2_CORE_BPE._encode_native("000", allowed_special).0, vec![830]);
        assert_eq!(GPT2_CORE_BPE._encode_native("0000", allowed_special).0, vec![2388]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00000", allowed_special).0, vec![20483]);
        assert_eq!(GPT2_CORE_BPE._encode_native("000000", allowed_special).0, vec![10535]);
        assert_eq!(GPT2_CORE_BPE._encode_native("0000000", allowed_special).0, vec![24598]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00000000", allowed_special).0, vec![8269]);
        assert_eq!(GPT2_CORE_BPE._encode_native("000000000", allowed_special).0, vec![10535, 830]);
        assert_eq!(GPT2_CORE_BPE._encode_native("0000000000", allowed_special).0, vec![8269, 405]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00000000000", allowed_special).0, vec![8269, 830]);
        assert_eq!(GPT2_CORE_BPE._encode_native("000000000000", allowed_special).0, vec![8269, 2388]);
        assert_eq!(GPT2_CORE_BPE._encode_native("0000000000000", allowed_special).0, vec![8269, 20483]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00000000000000", allowed_special).0, vec![8269, 10535]);
        assert_eq!(GPT2_CORE_BPE._encode_native("000000000000000", allowed_special).0, vec![8269, 24598]);
        assert_eq!(GPT2_CORE_BPE._encode_native("0000000000000000", allowed_special).0, vec![25645]);
        assert_eq!(GPT2_CORE_BPE._encode_native("00000000000000000", allowed_special).0, vec![8269, 10535, 830]);
    }
}