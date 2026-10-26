//! `--jobs` values, converted as Bazel's `ResourceConverter` does: an
//! integer, or a keyword naming a host resource, optionally followed by an
//! operation, such as `HOST_CPUS*.5`.

/// The most jobs Bazel runs; larger values are lowered to it.
pub const MAX_JOBS: i64 = 5000;

/// What a `--jobs` value may be, as Bazel's errors describe it.
const SYNTAX: &str = r#"an integer, or a keyword ("auto", "HOST_CPUS", "HOST_RAM"), optionally followed by an operation ([-|*]<float>) eg. "auto", "HOST_CPUS*.5""#;

/// The number of jobs a `--jobs` value asks for, at most [`MAX_JOBS`], or
/// Bazel's message saying why it isn't valid.
pub fn convert_jobs(value: &str) -> Result<i64, String> {
    let jobs = match keyword_jobs(value)? {
        Some(jobs) => jobs,
        None => value.parse::<i32>().map_err(|_| syntax_error(value))?.into(),
    };
    if jobs < 1 {
        return Err(format!("Value '({jobs})' must be at least 1."));
    }
    Ok(jobs.min(MAX_JOBS))
}

fn syntax_error(value: &str) -> String {
    format!("Parameter '{value}' does not follow correct syntax. This flag takes {SYNTAX}.")
}

/// The jobs a keyword value asks for, or `None` if `value` isn't one.
fn keyword_jobs(value: &str) -> Result<Option<i64>, String> {
    let keywords: [(&str, fn() -> i64); 3] = [("auto", host_cpus), ("HOST_CPUS", host_cpus), ("HOST_RAM", host_ram_mb)];
    for (keyword, amount) in keywords {
        let Some(expression) = value.strip_prefix(keyword) else {
            continue;
        };
        if !is_expression(expression) {
            continue;
        }
        return apply(amount(), expression).map(Some);
    }
    Ok(None)
}

/// Whether `expression` is empty or an operation, by the pattern Bazel
/// matches it with: `[-*][0-9]?(?:.[0-9]+)?`, where `.` is any character.
fn is_expression(expression: &str) -> bool {
    if expression.is_empty() {
        return true;
    }
    let Some(operand) = expression.strip_prefix(['-', '*']) else {
        return false;
    };
    let any_then_digits = |text: &str| {
        let mut chars = text.chars();
        match chars.next() {
            None => true,
            Some(_) => {
                let digits = chars.as_str();
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            }
        }
    };
    let after_digit = operand.strip_prefix(|c: char| c.is_ascii_digit());
    any_then_digits(operand) || after_digit.is_some_and(any_then_digits)
}

/// Apply an operation to a resource's amount, rounding to the nearest
/// integer as Java's `Math.round` does.
fn apply(amount: i64, expression: &str) -> Result<i64, String> {
    let Some(operator) = expression.chars().next() else {
        return Ok(amount);
    };
    let operand = &expression[1..];
    let Ok(operand) = operand.trim().parse::<f64>() else {
        return Err(format!("'{operand} is not a float"));
    };
    let result = match operator {
        '-' => amount as f64 - operand,
        _ => amount as f64 * operand,
    };
    Ok((result + 0.5).floor() as i64)
}

/// The CPUs this process may use.
fn host_cpus() -> i64 {
    std::thread::available_parallelism().map_or(1, |cpus| cpus.get() as i64)
}

/// The host's memory in megabytes. If it can't be read it's taken to be
/// unlimited, which for jobs means [`MAX_JOBS`].
fn host_ram_mb() -> i64 {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return i64::MAX;
    };
    let total_kb = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<i64>().ok());
    total_kb.map_or(i64::MAX, |kb| kb / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_integers_up_to_the_limit() {
        assert_eq!(convert_jobs("4"), Ok(4));
        assert_eq!(convert_jobs("+4"), Ok(4));
        assert_eq!(convert_jobs("6000"), Ok(MAX_JOBS));
        assert_eq!(convert_jobs("0"), Err("Value '(0)' must be at least 1.".to_string()));
        assert_eq!(convert_jobs("-1"), Err("Value '(-1)' must be at least 1.".to_string()));
    }

    #[test]
    fn takes_keywords_with_operations() {
        let cpus = host_cpus();
        assert_eq!(convert_jobs("auto"), Ok(cpus.min(MAX_JOBS)));
        assert_eq!(convert_jobs("HOST_CPUS*100"), Ok((cpus * 100).min(MAX_JOBS)));
        assert_eq!(convert_jobs(&format!("HOST_CPUS-{cpus}")), Err("Value '(0)' must be at least 1.".to_string()));
        assert_eq!(convert_jobs("HOST_RAM"), Ok(MAX_JOBS));
    }

    #[test]
    fn rejects_what_bazel_does() {
        for value in ["abc", "2.5", " 3", "HOST_CPUS/2", "auto*0.5x", "HOST_CPUS*1.5.5"] {
            assert_eq!(convert_jobs(value), Err(syntax_error(value)), "{value}");
        }
        assert_eq!(convert_jobs("auto*"), Err("' is not a float".to_string()));
    }

    #[test]
    fn rounds_operations_to_the_nearest_job() {
        assert_eq!(apply(48, "*0.02"), Ok(1));
        assert_eq!(apply(48, "*0.01"), Ok(0));
        assert_eq!(apply(4, "-1.5"), Ok(3));
        assert_eq!(apply(4, ""), Ok(4));
    }
}
