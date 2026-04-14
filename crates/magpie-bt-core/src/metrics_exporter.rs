//! Prometheus text-format metrics exporter (M2 observability gate).
//!
//! Gated behind the `prometheus` Cargo feature. Pure-stdlib renderer — no
//! `prometheus` crate dependency, so enabling the feature adds no
//! transitive build cost. Consumers wire the rendered text into their own
//! HTTP scrape endpoint (e.g. an axum handler returning
//! `Content-Type: text/plain; version=0.0.4`).
//!
//! Format reference:
//! <https://prometheus.io/docs/instrumenting/exposition_formats/#text-based-format>
//!
//! What's exposed today:
//! - `magpie_disk_*` counters from [`crate::session::disk::DiskMetrics`].
//!
//! Roadmap (out of M2 scope; tracked by follow-ups when those subsystems
//! expose stable counter names):
//! - `magpie_choker_*` (rotation count, slot occupancy).
//! - `magpie_shaper_*` (bytes consumed/denied per tier).
//! - `magpie_peer_*` (connected/disconnected counts per torrent).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use crate::session::disk::DiskMetrics;

/// Render a single torrent's disk metrics as Prometheus text-format.
///
/// `torrent_label` is interpolated as the `torrent` label so a consumer can
/// scrape multiple torrents into the same registry without name clashes.
/// Caller is responsible for sanitising the label (typically the lowercase
/// hex info-hash). Bytes counters are exposed as
/// [counters](https://prometheus.io/docs/concepts/metric_types/#counter)
/// (monotonic, never reset).
#[must_use]
pub fn render_disk_metrics(torrent_label: &str, m: &DiskMetrics) -> String {
    let label = escape_label(torrent_label);
    let mut out = String::with_capacity(512);
    let _ = writeln!(
        out,
        "# HELP magpie_disk_pieces_written Total pieces verified and persisted to storage."
    );
    let _ = writeln!(out, "# TYPE magpie_disk_pieces_written counter");
    let _ = writeln!(
        out,
        r#"magpie_disk_pieces_written{{torrent="{label}"}} {}"#,
        m.pieces_written.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP magpie_disk_bytes_written Total bytes written to storage across all verified pieces."
    );
    let _ = writeln!(out, "# TYPE magpie_disk_bytes_written counter");
    let _ = writeln!(
        out,
        r#"magpie_disk_bytes_written{{torrent="{label}"}} {}"#,
        m.bytes_written.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP magpie_disk_piece_verify_fail Pieces whose hash did not match after disk write."
    );
    let _ = writeln!(out, "# TYPE magpie_disk_piece_verify_fail counter");
    let _ = writeln!(
        out,
        r#"magpie_disk_piece_verify_fail{{torrent="{label}"}} {}"#,
        m.piece_verify_fail.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP magpie_disk_io_failures Disk I/O errors observed (read or write)."
    );
    let _ = writeln!(out, "# TYPE magpie_disk_io_failures counter");
    let _ = writeln!(
        out,
        r#"magpie_disk_io_failures{{torrent="{label}"}} {}"#,
        m.io_failures.load(Ordering::Relaxed)
    );
    out
}

/// Per the Prometheus text format spec: backslash, double-quote, and
/// newline must be escaped inside label values. Anything else passes
/// through. Conservative — caller should still avoid putting
/// untrusted-input bytes into a label.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn metrics(p: u64, b: u64, vf: u64, io: u64) -> DiskMetrics {
        DiskMetrics {
            pieces_written: AtomicU64::new(p),
            bytes_written: AtomicU64::new(b),
            piece_verify_fail: AtomicU64::new(vf),
            io_failures: AtomicU64::new(io),
        }
    }

    #[test]
    fn renders_each_counter_with_help_and_type() {
        let m = metrics(7, 1024, 1, 2);
        let text = render_disk_metrics("torrent42", &m);
        // Each counter has HELP, TYPE, and a value line.
        for name in &[
            "magpie_disk_pieces_written",
            "magpie_disk_bytes_written",
            "magpie_disk_piece_verify_fail",
            "magpie_disk_io_failures",
        ] {
            assert!(text.contains(&format!("# HELP {name}")), "missing HELP for {name}");
            assert!(text.contains(&format!("# TYPE {name} counter")), "missing TYPE for {name}");
            assert!(
                text.contains(&format!(r#"{name}{{torrent="torrent42"}}"#)),
                "missing value line for {name}"
            );
        }
        // Values are exactly what the atomics held.
        assert!(text.contains(r#"magpie_disk_pieces_written{torrent="torrent42"} 7"#));
        assert!(text.contains(r#"magpie_disk_bytes_written{torrent="torrent42"} 1024"#));
        assert!(text.contains(r#"magpie_disk_piece_verify_fail{torrent="torrent42"} 1"#));
        assert!(text.contains(r#"magpie_disk_io_failures{torrent="torrent42"} 2"#));
    }

    #[test]
    fn label_special_chars_are_escaped() {
        // Per Prometheus text format: backslash, double-quote, newline
        // must be escaped. The renderer takes care of this so consumers
        // don't accidentally produce malformed exposition. Test using a
        // real newline char to verify the newline escape path.
        let m = metrics(0, 0, 0, 0);
        let raw = "weird\"\\name\nhere"; // ", \, then a real newline
        let text = render_disk_metrics(raw, &m);
        // Each special char appears in its escaped form.
        assert!(text.contains("weird\\\"\\\\name\\nhere"), "got: {text}");
        // And the raw newline does NOT leak into the exposition (would
        // break a Prometheus parser mid-line).
        assert!(!text.contains(raw), "raw newline must not appear");
    }

    #[test]
    fn produces_valid_text_format_lines() {
        // Each non-comment line must match `<name>{<labels>} <value>`.
        // Comments start with `#`. No blank lines in the middle.
        let m = metrics(1, 2, 3, 4);
        let text = render_disk_metrics("t", &m);
        for line in text.lines() {
            assert!(!line.is_empty(), "no blank lines allowed");
            if line.starts_with('#') {
                assert!(
                    line.starts_with("# HELP ") || line.starts_with("# TYPE "),
                    "comment must be HELP or TYPE: {line:?}"
                );
            } else {
                // value line: name{labels} value
                assert!(line.contains('{') && line.contains('}'));
                let value: u64 = line
                    .rsplit(' ')
                    .next()
                    .expect("value present")
                    .parse()
                    .expect("value parses as u64");
                let _ = value; // just check it parses
            }
        }
    }
}
