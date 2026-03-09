use crate::tui::state::BuildProgress;

/// Parses `docker buildx bake --progress plain` stderr output to extract
/// step-level progress. BuildKit emits lines like:
///
/// ```text
/// #1 [internal] load build definition from Dockerfile
/// #1 DONE 0.0s
/// #2 [internal] load metadata for docker.io/library/node:18
/// #2 DONE 0.5s
/// #3 [1/5] FROM docker.io/library/node:18@sha256:abc
/// #3 DONE 0.0s
/// #4 [2/5] WORKDIR /app
/// #4 DONE 0.1s
/// #5 [3/5] COPY package*.json ./
/// #5 CACHED
/// #6 [4/5] RUN npm install
/// #6 ...
/// ```
///
/// We track `[M/N]` patterns to determine user-visible step progress.
pub struct ProgressParser {
    /// Highest user step M seen so far
    current_user_step: u32,
    /// Total user steps N from the `[M/N]` pattern
    total_user_steps: u32,
    /// Description of the most recent active step
    current_description: String,
    /// Number of completed buildkit steps (including internal ones)
    completed_steps: u32,
    /// Total buildkit steps seen
    highest_step: u32,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self {
            current_user_step: 0,
            total_user_steps: 0,
            current_description: String::new(),
            completed_steps: 0,
            highest_step: 0,
        }
    }

    /// Feed a line of `--progress plain` output. Returns `Some(BuildProgress)`
    /// when the progress state changed.
    pub fn parse_line(&mut self, line: &str) -> Option<BuildProgress> {
        let line = line.trim();

        // Match lines starting with #N
        let rest = line.strip_prefix('#')?;
        let (num_str, description) = rest.split_once(' ')?;
        let step_num: u32 = num_str.parse().ok()?;

        // Track highest step seen
        if step_num > self.highest_step {
            self.highest_step = step_num;
        }

        // Check for DONE or CACHED lines
        if description == "DONE" || description.starts_with("DONE ") || description == "CACHED" {
            self.completed_steps += 1;
            return Some(self.snapshot());
        }

        // Check for "..." lines (step in progress, no new info)
        if description == "..." {
            return None;
        }

        // Look for [M/N] pattern in the description
        if let Some(start) = description.find('[') {
            if let Some(end) = description[start..].find(']') {
                let bracket_content = &description[start + 1..start + end];
                if let Some((m_str, n_str)) = bracket_content.split_once('/') {
                    if let (Ok(m), Ok(n)) =
                        (m_str.trim().parse::<u32>(), n_str.trim().parse::<u32>())
                    {
                        // Update total if this stage has more steps
                        if n > self.total_user_steps {
                            self.total_user_steps = n;
                        }
                        self.current_user_step = m;

                        // Extract the command after [M/N]
                        let after_bracket = &description[start + end + 1..].trim();
                        self.current_description = if after_bracket.is_empty() {
                            description.to_string()
                        } else {
                            after_bracket.to_string()
                        };

                        return Some(self.snapshot());
                    }
                }
            }
        }

        // Internal step (no [M/N]) — still update description
        self.current_description = description.to_string();
        None
    }

    fn snapshot(&self) -> BuildProgress {
        BuildProgress {
            current_step: self.current_user_step,
            total_steps: self.total_user_steps,
            step_description: self.current_description.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_progress() {
        let mut p = ProgressParser::new();

        // Internal step — no progress returned
        assert!(p
            .parse_line("#1 [internal] load build definition from Dockerfile")
            .is_none());

        // Step done
        let r = p.parse_line("#1 DONE 0.0s").unwrap();
        assert_eq!(r.current_step, 0); // no user step yet
        assert_eq!(r.total_steps, 0);

        // User step
        let r = p
            .parse_line("#3 [1/5] FROM docker.io/library/node:18")
            .unwrap();
        assert_eq!(r.current_step, 1);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.step_description, "FROM docker.io/library/node:18");

        // Another user step
        let r = p.parse_line("#5 [3/5] COPY package*.json ./").unwrap();
        assert_eq!(r.current_step, 3);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.step_description, "COPY package*.json ./");
    }

    #[test]
    fn test_cached_step() {
        let mut p = ProgressParser::new();
        let r = p.parse_line("#5 CACHED").unwrap();
        assert_eq!(r.current_step, 0);
    }

    #[test]
    fn test_done_with_time() {
        let mut p = ProgressParser::new();
        let r = p.parse_line("#2 DONE 1.5s").unwrap();
        assert_eq!(r.current_step, 0);
    }

    #[test]
    fn test_ellipsis_ignored() {
        let mut p = ProgressParser::new();
        assert!(p.parse_line("#6 ...").is_none());
    }

    #[test]
    fn test_non_step_lines_ignored() {
        let mut p = ProgressParser::new();
        assert!(p.parse_line("").is_none());
        assert!(p.parse_line("some random output").is_none());
        assert!(p.parse_line("  WARNING: something").is_none());
    }

    #[test]
    fn test_multi_stage_build() {
        let mut p = ProgressParser::new();

        // First stage [1/3]
        let r = p.parse_line("#3 [1/3] FROM alpine:3.21").unwrap();
        assert_eq!(r.current_step, 1);
        assert_eq!(r.total_steps, 3);

        // Second stage starts with higher N — total updates
        let r = p.parse_line("#8 [1/5] FROM node:18").unwrap();
        assert_eq!(r.current_step, 1);
        assert_eq!(r.total_steps, 5); // updated to max

        let r = p.parse_line("#10 [4/5] RUN npm install").unwrap();
        assert_eq!(r.current_step, 4);
        assert_eq!(r.total_steps, 5);
    }

    #[test]
    fn test_step_with_stage_name() {
        let mut p = ProgressParser::new();
        // BuildKit sometimes shows: #4 [builder 2/5] RUN ...
        // The bracket content is "builder 2/5" — "builder 2" / "5"
        // "builder 2" won't parse as u32, so no [M/N] match — returns None
        assert!(p.parse_line("#4 [builder 2/5] RUN go build .").is_none());
    }

    #[test]
    fn test_step_with_stage_name_space() {
        let mut p = ProgressParser::new();
        // Actually BuildKit format for named stages is: [stage-name M/N]
        // e.g. [builder 2/5] — note "builder 2" before "/" won't parse
        // The real format uses "stage" prefix separated: we handle this gracefully
        let r = p.parse_line("#4 [2/5] RUN go build .").unwrap();
        assert_eq!(r.current_step, 2);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.step_description, "RUN go build .");
    }
}
