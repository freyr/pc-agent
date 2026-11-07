use anyhow::{Context, Result, bail};

const PROC_STAT: &str = "/proc/stat";

/// Reads the boot time (seconds since the epoch) from `/proc/stat`.
/// Without a readable `/proc/stat` this returns an error.
pub fn read_btime() -> Result<i64> {
    let text =
        std::fs::read_to_string(PROC_STAT).with_context(|| format!("cannot read {PROC_STAT}"))?;
    parse_btime(&text)
}

/// Finds the `btime <seconds>` line in the text of `/proc/stat`.
pub fn parse_btime(stat: &str) -> Result<i64> {
    for line in stat.lines() {
        if let Some(value) = line.strip_prefix("btime ") {
            return value
                .trim()
                .parse()
                .with_context(|| format!("invalid btime value {value:?}"));
        }
    }
    bail!("no btime line in {PROC_STAT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
cpu  2255 34 2290 22625563 6290 127 456 0 0 0
cpu0 1132 34 1441 11311718 3675 127 438 0 0 0
intr 114930548 113199788 3 0 5 263 0 4
ctxt 1990473
btime 1787892895
processes 2915
procs_running 1
procs_blocked 0
softirq 183433 0 21755 12 39 1137 231 21459 2263
";

    #[test]
    fn parses_btime_line() {
        assert_eq!(parse_btime(FIXTURE).unwrap(), 1_787_892_895);
    }

    #[test]
    fn missing_btime_is_an_error() {
        assert!(parse_btime("cpu  1 2 3\nctxt 5\n").is_err());
    }

    #[test]
    fn malformed_btime_is_an_error() {
        assert!(parse_btime("btime abc\n").is_err());
    }
}
