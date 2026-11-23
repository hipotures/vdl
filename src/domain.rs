//! Shared application operations used by CLI, terminal, HTTP, and the scheduler.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::repository::{RepoError, Source, SourceRepository, display_service};

pub fn parse_source_numbers(values: &[String]) -> Result<Vec<usize>, RepoError> {
    let joined = values.join(" ");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for piece in joined
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .filter(|p| !p.is_empty())
    {
        let num: isize = piece.parse().map_err(|_| {
            RepoError::Value("source numbers must be positive integers".to_string())
        })?;
        if num < 1 {
            return Err(RepoError::Value(
                "source numbers must be positive integers".to_string(),
            ));
        }
        let idx = num as usize;
        if seen.insert(idx) {
            out.push(idx);
        }
    }
    if out.is_empty() {
        return Err(RepoError::Value(
            "at least one source number is required".to_string(),
        ));
    }
    Ok(out)
}

pub fn resolve_source_numbers(
    sources: &[Source],
    values: &[String],
) -> Result<Vec<Source>, RepoError> {
    let numbers = parse_source_numbers(values)?;
    let invalid: Vec<String> = numbers
        .iter()
        .filter(|&&n| n > sources.len())
        .map(usize::to_string)
        .collect();
    if !invalid.is_empty() {
        return Err(RepoError::Value(format!(
            "source number out of range: {}",
            invalid.join(", ")
        )));
    }
    Ok(numbers
        .into_iter()
        .map(|n| sources[n - 1].clone())
        .collect())
}

pub fn disable_sources(
    repository: &SourceRepository,
    sources: &[Source],
) -> Result<Vec<Source>, RepoError> {
    let mut disabled = Vec::new();
    for source in sources {
        if source.active {
            disabled.push(repository.disable(source)?);
        }
    }
    Ok(disabled)
}

pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn format_age(last_check: Option<f64>, now: Option<f64>) -> String {
    let Some(last) = last_check else {
        return "never".to_string();
    };
    let current = now.unwrap_or_else(now_unix);
    let diff = (current - last).floor();
    let seconds = if diff < 0.0 { 0u64 } else { diff as u64 };
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds / 60;
    let rem_s = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {rem_s:02}s ago");
    }
    let hours = minutes / 60;
    let rem_m = minutes % 60;
    if hours < 24 {
        return format!("{hours}h {rem_m:02}m ago");
    }
    let days = hours / 24;
    let rem_h = hours % 24;
    format!("{days}d {rem_h}h ago")
}

pub fn format_sources_table(sources: &[Source], numbers: Option<&[usize]>) -> String {
    let headers = ["#", "Service", "Account", "Last check", "State"];
    let default_nums: Vec<usize> = (1..=sources.len()).collect();
    let nums = numbers.unwrap_or(&default_nums);
    let rows: Vec<[String; 5]> = nums
        .iter()
        .zip(sources.iter())
        .map(|(&num, src)| {
            [
                num.to_string(),
                display_service(&src.service),
                src.account.clone(),
                format_age(src.last_check, None),
                if src.active {
                    "active".to_string()
                } else {
                    "inactive".to_string()
                },
            ]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
    ];
    for row in &rows {
        for (col, val) in row.iter().enumerate() {
            widths[col] = widths[col].max(val.chars().count());
        }
    }

    let format_line = |vals: [&str; 5]| -> String {
        let mut line = String::new();
        for (idx, val) in vals.iter().enumerate() {
            if idx > 0 {
                line.push_str("  ");
            }
            line.push_str(val);
            let pad = widths[idx].saturating_sub(val.chars().count());
            for _ in 0..pad {
                line.push(' ');
            }
        }
        line.trim_end().to_string()
    };

    let mut out = format_line(headers);
    for row in &rows {
        out.push('\n');
        out.push_str(&format_line([&row[0], &row[1], &row[2], &row[3], &row[4]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed_delimiters() {
        for input in [
            vec!["2 7 15".to_string()],
            vec!["2,7,15".to_string()],
            vec!["2;7;15".to_string()],
            vec!["2,".to_string(), "7".to_string(), "15".to_string()],
        ] {
            assert_eq!(parse_source_numbers(&input).unwrap(), vec![2, 7, 15]);
        }
    }

    #[test]
    fn formats_ages() {
        assert_eq!(format_age(None, Some(1000.0)), "never");
        assert_eq!(format_age(Some(995.0), Some(1000.0)), "5s ago");
        assert_eq!(format_age(Some(875.0), Some(1000.0)), "2m 05s ago");
        assert_eq!(
            format_age(Some(1000.0 - 7500.0), Some(1000.0)),
            "2h 05m ago"
        );
        assert_eq!(
            format_age(Some(1000.0 - 90000.0), Some(1000.0)),
            "1d 1h ago"
        );
    }
}
