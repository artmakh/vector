use std::{collections::HashMap, str};

use bytes::Bytes;
use regex::bytes::{CaptureLocations, Regex, RegexSet};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use vector_common::TimeZone;

use crate::{
    config::{DataType, Input, Output, TransformConfig, TransformContext, TransformDescription},
    event::{Event, Value},
    internal_events::{
        ParserConversionError, ParserMatchError, ParserMissingFieldError, ParserTargetExistsError,
    },
    schema,
    transforms::{FunctionTransform, OutputBuffer, Transform},
    types::{parse_check_conversion_map, Conversion},
};

#[derive(Debug, Derivative, Deserialize, Serialize, Clone)]
#[derivative(Default)]
#[serde(default, deny_unknown_fields)]
pub struct RegexParserConfig {
    /// Deprecated. Use `patterns` instead.
    /// See #2469.
    /// TODO: Remove at a future point in time.
    pub regex: Option<String>,
    pub patterns: Vec<String>,
    pub field: Option<String>,
    #[derivative(Default(value = "true"))]
    pub drop_field: bool,
    pub drop_failed: bool,
    pub target_field: Option<String>,
    #[derivative(Default(value = "true"))]
    pub overwrite_target: bool,
    pub types: HashMap<String, String>,
    #[serde(default)]
    pub timezone: Option<TimeZone>,
}

inventory::submit! {
    TransformDescription::new::<RegexParserConfig>("regex_parser")
}

impl_generate_config_from_default!(RegexParserConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "regex_parser")]
impl TransformConfig for RegexParserConfig {
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        RegexParser::build(self, context.globals.timezone)
    }

    fn input(&self) -> Input {
        Input::log()
    }

    fn outputs(&self, _: &schema::Definition) -> Vec<Output> {
        vec![Output::default(DataType::Log)]
    }

    fn enable_concurrency(&self) -> bool {
        true
    }

    fn transform_type(&self) -> &'static str {
        "regex_parser"
    }
}

#[derive(Clone, Debug)]
pub struct RegexParser {
    regexset: RegexSet,
    patterns: Vec<CompiledRegex>, // indexes correspond to RegexSet
    field: String,
    drop_field: bool,
    drop_failed: bool,
    target_field: Option<String>,
    overwrite_target: bool,
}

#[derive(Debug, Clone)]
struct CompiledRegex {
    regex: Regex,
    capture_names: Vec<(usize, String, Conversion)>,
    capture_locs: CaptureLocations,
}

impl CompiledRegex {
    fn new(regex: Regex, types: &HashMap<String, Conversion>) -> CompiledRegex {
        // Calculate the location (index into the capture locations) of
        // each named capture, and the required type coercion.
        let capture_names = regex
            .capture_names()
            .enumerate()
            .filter_map(|(idx, cn)| {
                cn.map(|cn| {
                    let conv = types.get(cn).unwrap_or(&Conversion::Bytes);
                    let name = cn.to_string();
                    (idx, name, conv.clone())
                })
            })
            .collect::<Vec<_>>();
        let capture_locs = regex.capture_locations();
        CompiledRegex {
            regex,
            capture_names,
            capture_locs,
        }
    }

    /// Returns a list of captures (name, value) or None if the regex does not
    /// match
    fn captures<'a>(
        &'a mut self,
        value: &'a [u8],
    ) -> Option<impl Iterator<Item = (String, Value)> + 'a> {
        match self.regex.captures_read(&mut self.capture_locs, value) {
            Some(_) => {
                let capture_locs = &self.capture_locs;
                let values =
                    self.capture_names
                        .iter()
                        .filter_map(move |(idx, name, conversion)| {
                            capture_locs.get(*idx).and_then(|(start, end)| {
                                let capture = Bytes::from(value[start..end].to_owned());

                                match conversion.convert(capture) {
                                    Ok(value) => Some((name.clone(), value)),
                                    Err(error) => {
                                        emit!(ParserConversionError { name, error });
                                        None
                                    }
                                }
                            })
                        });
                Some(values)
            }
            None => {
                emit!(ParserMatchError { value });
                None
            }
        }
    }
}

impl RegexParser {
    pub fn build(config: &RegexParserConfig, timezone: TimeZone) -> crate::Result<Transform> {
        let field = config
            .field
            .clone()
            .unwrap_or_else(|| crate::config::log_schema().message_key().to_string());

        let patterns = match (&config.regex, &config.patterns.len()) {
            (None, 0) => {
                return Err(
                    "At least one regular expression must be defined, but `patterns` is empty."
                        .into(),
                );
            }
            (None, _) => config.patterns.clone(),
            (Some(regex), 0) => {
                // Still using the old `regex` syntax.
                // Printing a warning and wrapping input in a `vec`.
                warn!(
                    "Usage of `regex` is deprecated and will be removed in a future version. \
                     Please upgrade your config to use `patterns` instead: \
                     `patterns = ['{}']`. For more info, take a look at the documentation at \
                     https://vector.dev/docs/reference/transforms/regex_parser/.",
                    &regex
                );
                vec![regex.clone()]
            }
            _ => {
                return Err("`patterns = [...]` is not defined".into());
            }
        };

        let regexset = RegexSet::new(&patterns).context(super::InvalidRegexSnafu)?;

        // Pre-compile individual patterns
        let patterns: Result<Vec<Regex>, _> = regexset
            .patterns()
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect();
        let patterns = patterns.context(super::InvalidRegexSnafu)?;

        let names = &patterns
            .iter()
            .flat_map(|regex| regex.capture_names().flatten().collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let types =
            parse_check_conversion_map(&config.types, names, config.timezone.unwrap_or(timezone))?;

        // Build a buffer of the regex capture locations and names to avoid
        // repeated allocations.
        let patterns: Vec<CompiledRegex> = patterns
            .into_iter()
            .map(|regex| CompiledRegex::new(regex, &types))
            .collect();

        // Pre-calculate if the source field name should be dropped.
        let drop_field = config.drop_field
            && !patterns
                .iter()
                .flat_map(|p| &p.capture_names)
                .any(|(_, f, _)| *f == field);

        let parser = Self {
            regexset,
            patterns,
            field,
            drop_field,
            drop_failed: config.drop_failed,
            target_field: config.target_field.clone(),
            overwrite_target: config.overwrite_target,
        };
        Ok(Transform::function(parser))
    }
}

impl FunctionTransform for RegexParser {
    fn transform(&mut self, output: &mut OutputBuffer, mut event: Event) {
        let log = event.as_mut_log();
        let value = log.get(self.field.as_str()).map(|s| s.coerce_to_bytes());

        if let Some(value) = &value {
            let regex_id = self.regexset.matches(value).into_iter().next();
            let id = match regex_id {
                Some(id) => id,
                None => {
                    emit!(ParserMatchError { value });
                    if !self.drop_failed {
                        output.push(event);
                    };
                    return;
                }
            };

            let target_field = self.target_field.as_ref();

            let pattern = self
                .patterns
                .get_mut(id)
                .expect("Mismatch between capture patterns and regexset");

            if let Some(captures) = pattern.captures(value) {
                // Handle optional overwriting of the target field
                if let Some(target_field) = target_field {
                    if log.contains(target_field.as_str()) {
                        if self.overwrite_target {
                            log.remove(target_field.as_str());
                        } else {
                            emit!(ParserTargetExistsError { target_field });
                            output.push(event);
                            return;
                        }
                    }
                }

                log.extend(captures.map(|(name, value)| {
                    let name = target_field
                        .map(|target| format!("{}.{}", target, name))
                        .unwrap_or_else(|| name.clone());
                    (name, value)
                }));
                if self.drop_field {
                    log.remove(self.field.as_str());
                }
                output.push(event);
                return;
            }
        } else {
            emit!(ParserMissingFieldError { field: &self.field });
        }

        if !self.drop_failed {
            output.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use ordered_float::NotNan;

    use super::RegexParserConfig;
    use crate::{
        config::{TransformConfig, TransformContext},
        event::{Event, LogEvent, Value},
        transforms::OutputBuffer,
    };

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<RegexParserConfig>();
    }

    async fn do_transform(event: &str, patterns: &str, config: &str) -> Option<LogEvent> {
        let event = Event::from(event);
        let metadata = event.metadata().clone();
        let mut parser = toml::from_str::<RegexParserConfig>(&format!(
            r#"
                patterns = {}
                {}
            "#,
            patterns, config
        ))
        .unwrap()
        .build(&TransformContext::default())
        .await
        .unwrap();
        let parser = parser.as_function();

        let mut buf = OutputBuffer::with_capacity(1);
        parser.transform(&mut buf, event);
        let result = buf.into_events().next().map(|event| event.into_log());
        if let Some(event) = &result {
            assert_eq!(event.metadata(), &metadata);
        }
        result
    }

    #[tokio::test]
    async fn adds_parsed_field_to_event() {
        let log = do_transform(
            "status=1234 time=5678",
            r#"['status=(?P<status>\d+) time=(?P<time>\d+)']"#,
            "drop_field = false",
        )
        .await
        .unwrap();

        assert_eq!(log["status"], "1234".into());
        assert_eq!(log["time"], "5678".into());
        assert!(log.get("message").is_some());
    }

    #[tokio::test]
    async fn doesnt_do_anything_if_no_match() {
        let log = do_transform(
            "asdf1234",
            r#"['status=(?P<status>\d+)']"#,
            "drop_field = false",
        )
        .await
        .unwrap();

        assert_eq!(log.get("status"), None);
        assert!(log.get("message").is_some());
    }

    #[tokio::test]
    async fn does_drop_parsed_field() {
        let log = do_transform(
            "status=1234 time=5678",
            r#"['status=(?P<status>\d+) time=(?P<time>\d+)']"#,
            r#"field = "message""#,
        )
        .await
        .unwrap();

        assert_eq!(log["status"], "1234".into());
        assert_eq!(log["time"], "5678".into());
        assert!(log.get("message").is_none());
    }

    #[tokio::test]
    async fn does_not_drop_same_name_parsed_field() {
        let log = do_transform(
            "status=1234 message=yes",
            r#"['status=(?P<status>\d+) message=(?P<message>\S+)']"#,
            r#"field = "message""#,
        )
        .await
        .unwrap();

        assert_eq!(log["status"], "1234".into());
        assert_eq!(log["message"], "yes".into());
    }

    #[tokio::test]
    async fn does_not_drop_field_if_no_match() {
        let log = do_transform(
            "asdf1234",
            r#"['status=(?P<message>\S+)']"#,
            r#"field = "message""#,
        )
        .await
        .unwrap();

        assert!(log.get("message").is_some());
    }

    #[tokio::test]
    async fn respects_target_field() {
        let mut log = do_transform(
            "status=1234 time=5678",
            r#"['status=(?P<status>\d+) time=(?P<time>\d+)']"#,
            r#"
               target_field = "prefix"
               drop_field = false
            "#,
        )
        .await
        .unwrap();

        // timestamp is unpredictable, don't compare it
        log.remove("timestamp");
        let log = serde_json::to_value(log.all_fields().unwrap()).unwrap();
        assert_eq!(
            log,
            serde_json::json!({
                "message": "status=1234 time=5678",
                "prefix.status": "1234",
                "prefix.time": "5678",
            })
        );
    }

    #[tokio::test]
    async fn preserves_target_field() {
        let message = "status=1234 time=5678";
        let log = do_transform(
            message,
            r#"['status=(?P<status>\d+) time=(?P<time>\d+)']"#,
            r#"
               target_field = "message"
               overwrite_target = false
            "#,
        )
        .await
        .unwrap();

        assert_eq!(log["message"], message.into());
        assert_eq!(log.get("message.status"), None);
        assert_eq!(log.get("message.time"), None);
    }

    #[tokio::test]
    async fn overwrites_target_field() {
        let mut log = do_transform(
            "status=1234 time=5678",
            r#"['status=(?P<status>\d+) time=(?P<time>\d+)']"#,
            r#"
               target_field = "message"
               drop_field = false
            "#,
        )
        .await
        .unwrap();

        // timestamp is unpredictable, don't compare it
        log.remove("timestamp");
        let log = serde_json::to_value(log.all_fields().unwrap()).unwrap();
        assert_eq!(
            log,
            serde_json::json!({
                "message.status": "1234",
                "message.time": "5678",
            })
        );
    }

    #[tokio::test]
    async fn does_not_drop_event_if_match() {
        let log = do_transform("asdf1234", r#"['asdf']"#, "drop_failed = true").await;
        assert!(log.is_some());
    }

    #[tokio::test]
    async fn does_drop_event_if_no_match() {
        let log = do_transform("asdf1234", r#"['something']"#, "drop_failed = true").await;
        assert!(log.is_none());
    }

    #[tokio::test]
    async fn handles_valid_optional_capture() {
        let log = do_transform("1234", r#"['(?P<status>\d+)?']"#, "")
            .await
            .unwrap();
        assert_eq!(log["status"], "1234".into());
    }

    #[tokio::test]
    async fn handles_missing_optional_capture() {
        let log = do_transform("none", r#"['(?P<status>\d+)?']"#, "")
            .await
            .unwrap();
        assert!(log.get("status").is_none());
    }

    #[tokio::test]
    async fn coerces_fields_to_types() {
        let log = do_transform(
            "1234 6789.01 false",
            r#"['(?P<status>\d+) (?P<time>[\d.]+) (?P<check>\S+)']"#,
            r#"
            [types]
            status = "int"
            time = "float"
            check = "boolean"
            "#,
        )
        .await
        .expect("Failed to parse log");
        assert_eq!(log["check"], Value::Boolean(false));
        assert_eq!(log["status"], Value::Integer(1234));
        assert_eq!(log["time"], Value::Float(NotNan::new(6789.01).unwrap()));
    }

    #[tokio::test]
    async fn chooses_first_of_multiple_matching_patterns() {
        let log = do_transform(
            "1234 235.42 true",
            r#"[
                '^(?P<id1>\d+)',
                '^(?P<id2>\d+) (?P<time>[\d.]+) (?P<check>\S+)$',
            ]"#,
            r#"
            drop_field = false
            [types]
            id1 = "int"
            id2 = "int"
            time = "float"
            check = "boolean"
            "#,
        )
        .await
        .unwrap();

        assert_eq!(log["id1"], Value::Integer(1234));
        assert_eq!(log.get("id2"), None);
        assert_eq!(log.get("time"), None);
        assert_eq!(log.get("check"), None);
        assert!(log.get("message").is_some());
    }

    #[tokio::test]
    // https://github.com/vectordotdev/vector/issues/3096
    async fn correctly_maps_capture_groups_if_matching_pattern_is_not_first() {
        let log = do_transform(
            "match1234 235.42 true",
            r#"[
                '^nomatch(?P<id1>\d+)$',
                '^match(?P<id2>\d+) (?P<time>[\d.]+) (?P<check>\S+)$',
            ]"#,
            r#"
            drop_field = false
            [types]
            id1 = "int"
            id2 = "int"
            time = "float"
            check = "boolean"
            "#,
        )
        .await
        .unwrap();

        assert_eq!(log.get("id1"), None);
        assert_eq!(log["id2"], Value::Integer(1234));
        assert_eq!(log["time"], Value::Float(NotNan::new(235.42).unwrap()));
        assert_eq!(log["check"], Value::Boolean(true));
        assert!(log.get("message").is_some());
    }
}
