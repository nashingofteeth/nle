//! Reconstruct an editable timeline from detected cuts + black intervals.
//!
//! This is the *decision* half of "edit detect": `nle-ffmpeg::detect` runs
//! ffmpeg and hands back raw times, and this module turns them into exact,
//! frame-snapped media/gap spans. No processes and no clip ids here — the
//! caller owns the media pool and id allocation — so the whole thing is a
//! pure function that can be unit-tested exhaustively.
//!
//! A rendered file only preserves the *edit structure*, never the original
//! source media: every `Span::Media` points back into the same rendered
//! file, cut at the detected boundaries.

use crate::time::{Seconds, frames_to_seconds};

/// A reconstructed span of the rendered source.
#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    /// Playable material: the source range `[start, end)`.
    Media { start: Seconds, end: Seconds },
    /// Dead space (black + silence) of `duration`.
    Gap { duration: Seconds },
}

impl Span {
    pub fn duration(&self) -> Seconds {
        match self {
            Span::Media { start, end } => *end - *start,
            Span::Gap { duration } => *duration,
        }
    }
}

/// Snap `t` to the nearest 0-based frame boundary at `fps`. Exact-rational
/// math — floats at the detector boundary would drift across many cuts.
fn snap(t: Seconds, fps: Seconds) -> Seconds {
    let n = (t * fps).round().to_integer();
    frames_to_seconds(n, fps)
}

/// Build the spans of a rendered file from its detected `cuts` (media→media
/// boundaries) and `blacks` (black intervals), at `fps`.
///
/// The spans tile `[0, duration)` exactly: no overlaps, no gaps between
/// them, no zero-length pieces. `min_shot` folds a media span shorter than
/// that into a neighbouring media span (a flash that tripped the detector
/// twice); a short media span flanked only by gaps is left alone — it is
/// real material.
pub fn reconstruct(
    cuts: &[Seconds],
    blacks: &[(Seconds, Seconds)],
    duration: Seconds,
    fps: Seconds,
    min_shot: Seconds,
) -> Vec<Span> {
    let zero = Seconds::from_integer(0);
    if fps <= zero {
        return Vec::new();
    }
    let end = snap(duration, fps);
    if end <= zero {
        return Vec::new();
    }

    // Snap black intervals to the frame grid, clamp to the file, drop the
    // degenerate, then merge overlapping/adjacent ones.
    let mut gaps: Vec<(Seconds, Seconds)> = blacks
        .iter()
        .map(|(a, b)| (snap(*a, fps).max(zero), snap(*b, fps).min(end)))
        .filter(|(a, b)| b > a)
        .collect();
    gaps.sort();
    let mut merged: Vec<(Seconds, Seconds)> = Vec::new();
    for (a, b) in gaps {
        match merged.last_mut() {
            Some((_, prev_b)) if a <= *prev_b => *prev_b = (*prev_b).max(b),
            _ => merged.push((a, b)),
        }
    }

    // Every place a new segment could start: the file ends, each gap edge,
    // and each cut. A cut inside a black interval lands between two gap
    // edges, so it simply yields more gap that `coalesce_gaps` folds away.
    let mut bounds: Vec<Seconds> = Vec::with_capacity(merged.len() * 2 + cuts.len() + 2);
    bounds.push(zero);
    bounds.push(end);
    for (a, b) in &merged {
        bounds.push(*a);
        bounds.push(*b);
    }
    for c in cuts {
        let s = snap(*c, fps);
        if s > zero && s < end {
            bounds.push(s);
        }
    }
    bounds.sort();
    bounds.dedup();

    let in_gap = |t: Seconds| merged.iter().any(|(a, b)| *a <= t && t < *b);

    let mut spans: Vec<Span> = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if in_gap(a) {
            spans.push(Span::Gap { duration: b - a });
        } else {
            spans.push(Span::Media { start: a, end: b });
        }
    }

    merge_short_media(coalesce_gaps(spans), min_shot)
}

/// Fold consecutive gaps into one (a cut inside a black interval splits it).
fn coalesce_gaps(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match span {
            Span::Gap { duration: d } => match out.last_mut() {
                Some(Span::Gap { duration: prev }) => *prev += d,
                _ => out.push(Span::Gap { duration: d }),
            },
            other => out.push(other),
        }
    }
    out
}

/// Fold media spans shorter than `min_shot` into an adjacent media span.
fn merge_short_media(mut spans: Vec<Span>, min_shot: Seconds) -> Vec<Span> {
    if min_shot <= Seconds::from_integer(0) {
        return spans;
    }
    let mut i = 0;
    while i < spans.len() {
        let short = matches!(&spans[i], Span::Media { .. }) && spans[i].duration() < min_shot;
        if !short {
            i += 1;
            continue;
        }
        let prev_media = i > 0 && matches!(&spans[i - 1], Span::Media { .. });
        let next_media = i + 1 < spans.len() && matches!(&spans[i + 1], Span::Media { .. });
        if prev_media {
            // Swallow this span by extending the previous media's out-point.
            let Span::Media { end, .. } = &spans[i] else {
                unreachable!()
            };
            let end = *end;
            let Span::Media { end: prev_end, .. } = &mut spans[i - 1] else {
                unreachable!()
            };
            *prev_end = end;
            spans.remove(i);
            i = i.saturating_sub(1);
        } else if next_media {
            // Prepend into the following media: move its in-point back.
            let Span::Media { start, .. } = &spans[i] else {
                unreachable!()
            };
            let start = *start;
            let Span::Media {
                start: next_start, ..
            } = &mut spans[i + 1]
            else {
                unreachable!()
            };
            *next_start = start;
            spans.remove(i);
        } else {
            // Flanked only by gaps (or nothing): real material, keep it.
            i += 1;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    #[test]
    fn no_cuts_is_one_shot() {
        let spans = reconstruct(&[], &[], secs(5), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![Span::Media {
                start: secs(0),
                end: secs(5)
            }]
        );
    }

    #[test]
    fn cuts_split_into_clips() {
        let spans = reconstruct(&[secs(2), secs(4)], &[], secs(6), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(2)
                },
                Span::Media {
                    start: secs(2),
                    end: secs(4)
                },
                Span::Media {
                    start: secs(4),
                    end: secs(6)
                },
            ]
        );
    }

    #[test]
    fn black_intervals_become_gaps() {
        let spans = reconstruct(&[secs(2)], &[(secs(3), secs(4))], secs(5), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(2)
                },
                Span::Media {
                    start: secs(2),
                    end: secs(3)
                },
                Span::Gap { duration: secs(1) },
                Span::Media {
                    start: secs(4),
                    end: secs(5)
                },
            ]
        );
    }

    #[test]
    fn cut_inside_black_is_absorbed_into_one_gap() {
        // A cut at 3.5 lands inside the black interval [3,4): the two gap
        // pieces coalesce, so no cut ever splits a gap.
        let spans = reconstruct(&[secs(3)], &[(secs(3), secs(4))], secs(5), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(3)
                },
                Span::Gap { duration: secs(1) },
                Span::Media {
                    start: secs(4),
                    end: secs(5)
                },
            ]
        );
    }

    #[test]
    fn leading_and_trailing_black_are_gaps() {
        let spans = reconstruct(
            &[],
            &[(secs(0), secs(1)), (secs(4), secs(5))],
            secs(5),
            secs(1),
            secs(0),
        );
        assert_eq!(
            spans,
            vec![
                Span::Gap { duration: secs(1) },
                Span::Media {
                    start: secs(1),
                    end: secs(4)
                },
                Span::Gap { duration: secs(1) },
            ]
        );
    }

    #[test]
    fn min_shot_merges_a_flash_back_into_the_shot() {
        // A spurious cut 0.4s after a real one (5fps so the half-second is
        // representable): with min_shot 1 the tiny middle shot is swallowed
        // by the previous one.
        let spans = reconstruct(
            &[secs(2), Seconds::new(12, 5)],
            &[],
            secs(5),
            secs(5),
            secs(1),
        );
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: Seconds::new(12, 5)
                },
                Span::Media {
                    start: Seconds::new(12, 5),
                    end: secs(5)
                },
            ]
        );
    }

    #[test]
    fn min_shot_prepends_into_the_next_shot_at_the_head() {
        // The short piece is first, so there is no previous media to grow;
        // it merges forward instead. 5fps keeps the 0.4s cut on the grid.
        let spans = reconstruct(&[Seconds::new(2, 5)], &[], secs(5), secs(5), secs(1));
        assert_eq!(
            spans,
            vec![Span::Media {
                start: secs(0),
                end: secs(5)
            }]
        );
    }

    #[test]
    fn a_short_shot_between_gaps_is_kept() {
        // Not a flash: black on both sides means the material is real.
        // 2fps keeps the half-second gap edges on the grid.
        let spans = reconstruct(
            &[],
            &[(secs(1), secs(2)), (Seconds::new(5, 2), secs(3))],
            secs(4),
            secs(2),
            secs(1),
        );
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(1)
                },
                Span::Gap { duration: secs(1) },
                Span::Media {
                    start: secs(2),
                    end: Seconds::new(5, 2)
                },
                Span::Gap {
                    duration: Seconds::new(1, 2)
                },
                Span::Media {
                    start: secs(3),
                    end: secs(4)
                },
            ]
        );
    }

    #[test]
    fn cuts_are_snapped_to_the_frame_grid() {
        // 0.99s at 30fps is frame 29.7 → frame 30 = 1.0s.
        let spans = reconstruct(&[Seconds::new(99, 100)], &[], secs(3), secs(30), secs(0));
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(1)
                },
                Span::Media {
                    start: secs(1),
                    end: secs(3)
                },
            ]
        );
    }

    #[test]
    fn duration_is_snapped_to_the_last_frame() {
        // 5.02s at 25fps is 125.5 frames → 126 → 5.04s.
        let spans = reconstruct(&[], &[], Seconds::new(502, 100), secs(25), secs(0));
        assert_eq!(
            spans,
            vec![Span::Media {
                start: secs(0),
                end: Seconds::new(126, 25)
            }]
        );
    }

    #[test]
    fn out_of_range_cuts_are_ignored() {
        let spans = reconstruct(&[secs(0), secs(6), secs(9)], &[], secs(6), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![Span::Media {
                start: secs(0),
                end: secs(6)
            }]
        );
    }

    #[test]
    fn black_past_the_end_is_clamped() {
        let spans = reconstruct(&[], &[(secs(4), secs(10))], secs(5), secs(1), secs(0));
        assert_eq!(
            spans,
            vec![
                Span::Media {
                    start: secs(0),
                    end: secs(4)
                },
                Span::Gap { duration: secs(1) },
            ]
        );
    }

    #[test]
    fn zero_duration_is_empty() {
        assert!(reconstruct(&[], &[], secs(0), secs(1), secs(0)).is_empty());
    }

    #[test]
    fn spans_tile_the_file_exactly() {
        let spans = reconstruct(
            &[secs(2), Seconds::new(9, 2)],
            &[(secs(3), secs(4))],
            secs(6),
            secs(1),
            Seconds::new(1, 2),
        );
        let total: Seconds = spans.iter().map(Span::duration).sum();
        assert_eq!(total, secs(6));
        assert!(spans.iter().all(|s| s.duration() > secs(0)));
    }
}
