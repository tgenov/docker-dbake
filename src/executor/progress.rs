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
    /// Stage name from the most recent `[stage M/N]` line, if any
    current_stage: Option<String>,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self {
            current_user_step: 0,
            total_user_steps: 0,
            current_description: String::new(),
            current_stage: None,
        }
    }

    /// Feed a line of `--progress plain` output. Returns `Some(BuildProgress)`
    /// when the progress state changed.
    pub fn parse_line(&mut self, line: &str) -> Option<BuildProgress> {
        let line = line.trim();

        // Match lines starting with #N
        let rest = line.strip_prefix('#')?;
        let (num_str, description) = rest.split_once(' ')?;
        let _step_num: u32 = num_str.parse().ok()?;

        // Check for DONE or CACHED lines
        if description == "DONE" || description.starts_with("DONE ") || description == "CACHED" {
            return Some(self.snapshot());
        }

        // Check for "..." lines (step in progress, no new info)
        if description == "..." {
            return None;
        }

        // Look for a [M/N] or [stage M/N] pattern in the description
        if let Some(start) = description.find('[') {
            if let Some(end) = description[start..].find(']') {
                let bracket_content = &description[start + 1..start + end];
                // BuildKit prefixes the counter with the stage name whenever a
                // Dockerfile has more than one stage: `[builder 2/5]`.
                let (stage, counter) = match bracket_content.rsplit_once(' ') {
                    Some((stage, counter)) => (Some(stage.trim()), counter),
                    None => (None, bracket_content),
                };
                if let Some((m_str, n_str)) = counter.split_once('/') {
                    if let (Ok(m), Ok(n)) =
                        (m_str.trim().parse::<u32>(), n_str.trim().parse::<u32>())
                    {
                        self.current_stage = stage.filter(|s| !s.is_empty()).map(str::to_string);
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
            stage: self.current_stage.clone(),
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
        // BuildKit prefixes the counter with the stage name for any Dockerfile
        // with more than one stage: `#4 [builder 2/5] RUN ...`.
        let mut p = ProgressParser::new();
        let r = p.parse_line("#4 [builder 2/5] RUN go build .").unwrap();
        assert_eq!(r.current_step, 2);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.step_description, "RUN go build .");
        assert_eq!(r.stage.as_deref(), Some("builder"));
    }

    #[test]
    fn test_stage_name_containing_digits() {
        let mut p = ProgressParser::new();
        let r = p.parse_line("#7 [go1-22 3/9] RUN make").unwrap();
        assert_eq!(r.current_step, 3);
        assert_eq!(r.total_steps, 9);
        assert_eq!(r.stage.as_deref(), Some("go1-22"));
    }

    #[test]
    fn test_multi_platform_stage_prefix() {
        // BuildKit prefixes the platform on multi-platform builds, so the
        // bracket holds TWO spaces: `[linux/arm64 builder 3/8]`. Splitting on
        // the first space instead of the last silently kills all progress
        // reporting for every multi-platform build.
        let mut p = ProgressParser::new();
        let r = p
            .parse_line("#12 [linux/arm64 builder 3/8] RUN make")
            .unwrap();
        assert_eq!(r.current_step, 3);
        assert_eq!(r.total_steps, 8);
        assert_eq!(r.stage.as_deref(), Some("linux/arm64 builder"));
        assert_eq!(r.step_description, "RUN make");
    }

    #[test]
    fn test_default_stage_name() {
        let mut p = ProgressParser::new();
        let r = p.parse_line("#5 [stage-1 2/5] RUN apt-get update").unwrap();
        assert_eq!(r.current_step, 2);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.stage.as_deref(), Some("stage-1"));
    }

    #[test]
    fn test_unnamed_stage_has_no_stage_label() {
        let mut p = ProgressParser::new();
        let r = p.parse_line("#4 [2/5] RUN go build .").unwrap();
        assert_eq!(r.current_step, 2);
        assert_eq!(r.stage, None);
    }

    #[test]
    fn test_internal_bracket_is_not_a_step_counter() {
        let mut p = ProgressParser::new();
        assert!(p
            .parse_line("#1 [internal] load build definition from Dockerfile")
            .is_none());
    }

    #[test]
    fn test_step_with_stage_name_space() {
        let mut p = ProgressParser::new();
        // The unprefixed form, emitted for single-stage Dockerfiles.
        let r = p.parse_line("#4 [2/5] RUN go build .").unwrap();
        assert_eq!(r.current_step, 2);
        assert_eq!(r.total_steps, 5);
        assert_eq!(r.step_description, "RUN go build .");
    }
}
