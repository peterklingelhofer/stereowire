//! Forward error correction for video fragments.
//!
//! A video frame is split across many datagrams and losing any one of them
//! loses the whole frame, which then costs a resync and a fresh keyframe. A
//! keyframe is the worst case: it is the largest frame and the most expensive
//! to lose, and at a hundred-plus fragments even a 1% loss rate makes losing at
//! least one of them the likely outcome rather than the unlucky one.
//!
//! Each group of fragments therefore carries one parity datagram, the XOR of
//! its members, which rebuilds any single missing fragment in that group. That
//! turns the arithmetic around: instead of needing every fragment to survive,
//! each group only needs all but one.

use crate::packet::MAX_PAYLOAD;

/// Fragments protected by one group's parity. Ten costs 10% overhead per parity
/// block and, at realistic loss rates, is the difference between most large
/// frames arriving and most of them failing.
pub const GROUP: usize = 10;

/// Parity blocks a group may carry.
///
/// The first is a plain XOR, which rebuilds any single missing fragment. The
/// second is a weighted sum over GF(256), which is what makes *two* losses in
/// the same group recoverable: two plain XORs of the same fragments would carry
/// no more information than one. This is the P+Q scheme RAID-6 uses.
pub const MAX_PARITY: usize = 2;

/// GF(256) exponent and logarithm tables for the polynomial 0x11d, which is the
/// one Reed-Solomon implementations conventionally use.
const TABLES: (([u8; 512], [u8; 256]),) = (build_tables(),);

const fn build_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    // Doubling the exponent table lets multiplication skip a modulo.
    let mut i = 255;
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    (exp, log)
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let (exp, log) = &TABLES.0;
    exp[log[a as usize] as usize + log[b as usize] as usize]
}

fn gf_div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    debug_assert!(b != 0, "division by zero in GF(256)");
    let (exp, log) = &TABLES.0;
    exp[log[a as usize] as usize + 255 - log[b as usize] as usize]
}

/// The coefficient applied to the fragment at `index` when building Q.
///
/// Distinct non-zero coefficients are what make the P and Q equations
/// independent, and so solvable for two unknowns.
fn coefficient(index: usize) -> u8 {
    let (exp, _) = &TABLES.0;
    exp[index % 255]
}

/// Two bytes of every protected fragment carry its own length, so a rebuilt
/// fragment knows how long it is.
const LENGTH_PREFIX: usize = 2;

/// One byte on each parity datagram says which parity block it is, so the
/// receiver can tell P from Q however they arrive.
pub const LEVEL_PREFIX: usize = 1;

/// Data a protected fragment may carry. Sized so a parity datagram, which is a
/// full-width block plus its length and level bytes, still fits one datagram.
pub const BLOCK: usize = MAX_PAYLOAD - LENGTH_PREFIX - LEVEL_PREFIX;

/// A parity block is always full width: length prefix plus a whole block.
pub const PARITY_LEN: usize = LENGTH_PREFIX + BLOCK;

/// Writes `data` as a protected fragment: its length, then the bytes.
pub fn encode_fragment(data: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
}

/// Reads a protected fragment back, rejecting anything malformed.
pub fn decode_fragment(payload: &[u8]) -> Option<&[u8]> {
    let (prefix, rest) = payload.split_at_checked(LENGTH_PREFIX)?;
    let len = u16::from_le_bytes(prefix.try_into().ok()?) as usize;
    rest.get(..len)
}

/// Folds one fragment into a parity block, weighted for parity `level`.
///
/// Level 0 is a plain XOR (P). Level 1 weights each fragment by a distinct
/// GF(256) coefficient (Q). Fragments are combined as though padded to the full
/// block width, which lets a short final fragment sit in a group with
/// full-width ones.
pub fn accumulate_at(parity: &mut [u8; PARITY_LEN], data: &[u8], index: usize, level: usize) {
    if data.len() > BLOCK {
        return;
    }
    let weight = if level == 0 { 1 } else { coefficient(index) };
    let len = (data.len() as u16).to_le_bytes();
    parity[0] ^= gf_mul(len[0], weight);
    parity[1] ^= gf_mul(len[1], weight);
    for (slot, byte) in parity[LENGTH_PREFIX..].iter_mut().zip(data) {
        *slot ^= gf_mul(*byte, weight);
    }
}

/// Plain XOR parity, for a group's first parity block.
pub fn accumulate(parity: &mut [u8; PARITY_LEN], data: &[u8]) {
    accumulate_at(parity, data, 0, 0);
}

/// Reads a coded block back into a fragment, rejecting an implausible length.
fn decode_coded(block: &[u8; PARITY_LEN]) -> Option<Vec<u8>> {
    let len = u16::from_le_bytes([block[0], block[1]]) as usize;
    block
        .get(LENGTH_PREFIX..LENGTH_PREFIX + len)
        .map(<[u8]>::to_vec)
}

/// Rebuilds the missing fragments of a group from whatever parity survived.
///
/// `fragments` is the group in order, with gaps as `None`. `p` and `q` are the
/// group's parity blocks if they arrived. Returns how many were rebuilt.
///
/// Recovers one missing fragment from either parity block, and two from both.
pub fn recover_group(
    fragments: &mut [Option<Vec<u8>>],
    p: Option<&[u8]>,
    q: Option<&[u8]>,
) -> usize {
    let missing: Vec<usize> = fragments
        .iter()
        .enumerate()
        .filter(|(_, f)| f.is_none())
        .map(|(i, _)| i)
        .collect();

    let as_block = |bytes: Option<&[u8]>| -> Option<[u8; PARITY_LEN]> {
        bytes.and_then(|b| b.try_into().ok())
    };
    let (p, q) = (as_block(p), as_block(q));

    match missing.as_slice() {
        [] => 0,
        [only] => {
            // Either parity block alone is enough for a single gap.
            let (Some(parity), level) = (p.or(q), usize::from(p.is_none())) else {
                return 0;
            };
            let mut residual = parity;
            for (index, data) in fragments.iter().enumerate() {
                if let Some(data) = data {
                    accumulate_at(&mut residual, data, index, level);
                }
            }
            // Residual now holds the missing fragment scaled by its weight.
            if level == 1 {
                let weight = coefficient(*only);
                for byte in residual.iter_mut() {
                    *byte = gf_div(*byte, weight);
                }
            }
            match decode_coded(&residual) {
                Some(rebuilt) => {
                    fragments[*only] = Some(rebuilt);
                    1
                }
                None => 0,
            }
        }
        [first, second] => {
            let (Some(p), Some(q)) = (p, q) else { return 0 };
            let (mut a, mut b) = (p, q);
            for (index, data) in fragments.iter().enumerate() {
                if let Some(data) = data {
                    accumulate_at(&mut a, data, index, 0);
                    accumulate_at(&mut b, data, index, 1);
                }
            }
            // a = x ^ y, b = g_first*x ^ g_second*y, so
            // x = (b ^ g_second*a) / (g_first ^ g_second).
            let (gi, gj) = (coefficient(*first), coefficient(*second));
            let denominator = gi ^ gj;
            if denominator == 0 {
                return 0;
            }
            let mut x = [0u8; PARITY_LEN];
            let mut y = [0u8; PARITY_LEN];
            for k in 0..PARITY_LEN {
                x[k] = gf_div(b[k] ^ gf_mul(gj, a[k]), denominator);
                y[k] = a[k] ^ x[k];
            }
            match (decode_coded(&x), decode_coded(&y)) {
                (Some(first_data), Some(second_data)) => {
                    fragments[*first] = Some(first_data);
                    fragments[*second] = Some(second_data);
                    2
                }
                _ => 0,
            }
        }
        // Three or more gaps is beyond what two parity blocks can describe.
        _ => 0,
    }
}

/// Rebuilds the one fragment missing from a group.
///
/// `present` holds the group's surviving fragments, already decoded. Returns
/// `None` if the parity block is the wrong shape or the result is nonsense,
/// which is what a second loss in the same group looks like.
pub fn recover(parity: &[u8], present: &[&[u8]]) -> Option<Vec<u8>> {
    let mut block: [u8; PARITY_LEN] = parity.try_into().ok()?;
    for data in present {
        accumulate(&mut block, data);
    }
    let len = u16::from_le_bytes([block[0], block[1]]) as usize;
    // A length past the block width means more than one member was missing.
    block
        .get(LENGTH_PREFIX..LENGTH_PREFIX + len)
        .map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn a_fragment_survives_its_length_prefix() {
        for len in [0usize, 1, 500, BLOCK] {
            let data = fragment(7, len);
            let mut encoded = Vec::new();
            encode_fragment(&data, &mut encoded);
            assert!(encoded.len() <= MAX_PAYLOAD, "must fit one datagram");
            assert_eq!(decode_fragment(&encoded), Some(data.as_slice()));
        }
    }

    #[test]
    fn malformed_fragments_are_refused() {
        assert_eq!(decode_fragment(&[]), None);
        assert_eq!(decode_fragment(&[1]), None);
        // Claims 400 bytes but carries none.
        let mut lying = 400u16.to_le_bytes().to_vec();
        lying.extend_from_slice(&[0u8; 10]);
        assert_eq!(decode_fragment(&lying), None);
    }

    /// Builds a group's P and Q parity blocks.
    fn parity_for(group: &[Vec<u8>]) -> ([u8; PARITY_LEN], [u8; PARITY_LEN]) {
        let mut p = [0u8; PARITY_LEN];
        let mut q = [0u8; PARITY_LEN];
        for (index, data) in group.iter().enumerate() {
            accumulate_at(&mut p, data, index, 0);
            accumulate_at(&mut q, data, index, 1);
        }
        (p, q)
    }

    #[test]
    fn every_pair_of_losses_in_a_group_is_rebuilt_exactly() {
        // Mixed widths, including a short final fragment, since a real frame
        // rarely divides evenly.
        let group: Vec<Vec<u8>> = (0..GROUP)
            .map(|i| {
                fragment(
                    (i as u8).wrapping_mul(31),
                    if i == GROUP - 1 { 77 } else { BLOCK },
                )
            })
            .collect();
        let (p, q) = parity_for(&group);

        // Exhaustive: every pair of positions, which is the whole space this
        // scheme claims to cover.
        for first in 0..GROUP {
            for second in (first + 1)..GROUP {
                let mut fragments: Vec<Option<Vec<u8>>> = group.iter().cloned().map(Some).collect();
                fragments[first] = None;
                fragments[second] = None;

                let rebuilt = recover_group(&mut fragments, Some(&p), Some(&q));
                assert_eq!(
                    rebuilt, 2,
                    "losing {first} and {second} must be recoverable"
                );
                assert_eq!(
                    fragments[first].as_deref(),
                    Some(group[first].as_slice()),
                    "fragment {first} must come back byte for byte"
                );
                assert_eq!(fragments[second].as_deref(), Some(group[second].as_slice()));
            }
        }
    }

    #[test]
    fn a_single_loss_is_rebuilt_from_either_parity_block_alone() {
        let group: Vec<Vec<u8>> = (0..GROUP)
            .map(|i| fragment((i as u8).wrapping_mul(7), BLOCK))
            .collect();
        let (p, q) = parity_for(&group);

        for missing in 0..GROUP {
            // P survived, Q did not.
            let mut only_p: Vec<Option<Vec<u8>>> = group.iter().cloned().map(Some).collect();
            only_p[missing] = None;
            assert_eq!(recover_group(&mut only_p, Some(&p), None), 1);
            assert_eq!(only_p[missing].as_deref(), Some(group[missing].as_slice()));

            // Q survived, P did not. The weighted equation has to work alone.
            let mut only_q: Vec<Option<Vec<u8>>> = group.iter().cloned().map(Some).collect();
            only_q[missing] = None;
            assert_eq!(recover_group(&mut only_q, None, Some(&q)), 1);
            assert_eq!(
                only_q[missing].as_deref(),
                Some(group[missing].as_slice()),
                "the weighted parity must rebuild position {missing} on its own"
            );
        }
    }

    #[test]
    fn three_losses_are_refused_rather_than_guessed() {
        let group: Vec<Vec<u8>> = (0..GROUP).map(|i| fragment(i as u8, BLOCK)).collect();
        let (p, q) = parity_for(&group);
        let mut fragments: Vec<Option<Vec<u8>>> = group.iter().cloned().map(Some).collect();
        for gap in [2, 5, 8] {
            fragments[gap] = None;
        }
        assert_eq!(
            recover_group(&mut fragments, Some(&p), Some(&q)),
            0,
            "two parity blocks cannot describe three unknowns"
        );
        assert!(
            fragments[2].is_none(),
            "nothing plausible-looking may be invented"
        );
    }

    #[test]
    fn two_losses_with_only_one_parity_block_are_refused() {
        let group: Vec<Vec<u8>> = (0..GROUP).map(|i| fragment(i as u8, BLOCK)).collect();
        let (p, _) = parity_for(&group);
        let mut fragments: Vec<Option<Vec<u8>>> = group.iter().cloned().map(Some).collect();
        fragments[1] = None;
        fragments[4] = None;
        assert_eq!(recover_group(&mut fragments, Some(&p), None), 0);
    }

    #[test]
    fn the_field_arithmetic_is_self_consistent() {
        // Every non-zero value must divide back to itself, or recovery silently
        // produces rubbish.
        for a in 1u16..256 {
            for b in 1u16..256 {
                let product = gf_mul(a as u8, b as u8);
                assert_ne!(product, 0, "{a} times {b} must not vanish");
                assert_eq!(gf_div(product, b as u8), a as u8, "{a} times {b} over {b}");
            }
        }
        // Coefficients must be distinct across a group, otherwise the two
        // parity equations collapse into one.
        let weights: Vec<u8> = (0..GROUP).map(coefficient).collect();
        for i in 0..weights.len() {
            for j in (i + 1)..weights.len() {
                assert_ne!(weights[i], weights[j], "coefficients {i} and {j} collide");
            }
        }
    }

    #[test]
    fn any_single_missing_fragment_is_rebuilt_exactly() {
        let group: Vec<Vec<u8>> = (0..GROUP)
            .map(|i| fragment((i as u8).wrapping_mul(13), BLOCK))
            .collect();
        let mut parity = [0u8; PARITY_LEN];
        for data in &group {
            accumulate(&mut parity, data);
        }

        for missing in 0..GROUP {
            let present: Vec<&[u8]> = group
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != missing)
                .map(|(_, d)| d.as_slice())
                .collect();
            assert_eq!(
                recover(&parity, &present).as_deref(),
                Some(group[missing].as_slice()),
                "fragment {missing} must come back byte for byte"
            );
        }
    }

    #[test]
    fn a_short_final_fragment_is_rebuilt_at_its_true_length() {
        // Real frames rarely divide evenly, so the last fragment is short.
        let group = vec![fragment(1, BLOCK), fragment(2, BLOCK), fragment(3, 37)];
        let mut parity = [0u8; PARITY_LEN];
        for data in &group {
            accumulate(&mut parity, data);
        }
        let present: Vec<&[u8]> = vec![&group[0], &group[1]];
        let rebuilt = recover(&parity, &present).expect("rebuilds");
        assert_eq!(
            rebuilt.len(),
            37,
            "the recovered length must be the true one"
        );
        assert_eq!(rebuilt, group[2]);
    }

    #[test]
    fn two_losses_in_one_group_cannot_be_rebuilt() {
        let group: Vec<Vec<u8>> = (0..4).map(|i| fragment(i as u8, BLOCK)).collect();
        let mut parity = [0u8; PARITY_LEN];
        for data in &group {
            accumulate(&mut parity, data);
        }
        // Two absent: whatever comes out must not be mistaken for fragment 0.
        let present: Vec<&[u8]> = vec![&group[2], &group[3]];
        let rebuilt = recover(&parity, &present);
        assert_ne!(
            rebuilt.as_deref(),
            Some(group[0].as_slice()),
            "a second loss must not produce a plausible-looking fragment"
        );
    }

    /// A parity datagram must never need fragmenting: splitting the repair
    /// across datagrams would let a single loss destroy the repair as well.
    const _: () = assert!(LEVEL_PREFIX + PARITY_LEN <= MAX_PAYLOAD);
}
