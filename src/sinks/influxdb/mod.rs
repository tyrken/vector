pub mod logs;
pub mod metrics;

pub(self) use super::{Healthcheck, RouterSink};

use crate::{dns::Resolver, sinks::util::http2::HttpClient};
use chrono::{DateTime, Utc};
use futures::TryFutureExt;
use futures01::Future;
use http02::{StatusCode, Uri};
use hyper13;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use snafu::Snafu;
use std::collections::{BTreeMap, HashMap};
use tower03::Service;

pub enum Field {
    /// string
    String(String),
    /// float
    Float(f64),
    /// unsigned integer
    UnsignedInt(u32),
    /// integer
    Int(i64),
    /// boolean
    Bool(bool),
}

#[derive(Debug, Snafu)]
enum ConfigError {
    #[snafu(display("InfluxDB v1 or v2 should be configured as endpoint."))]
    MissingConfiguration,
    #[snafu(display(
        "Unclear settings. Both version configured v1: {:?}, v2: {:?}.",
        v1_settings,
        v2_settings
    ))]
    BothConfiguration {
        v1_settings: InfluxDB1Settings,
        v2_settings: InfluxDB2Settings,
    },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct InfluxDB1Settings {
    database: String,
    consistency: Option<String>,
    retention_policy_name: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct InfluxDB2Settings {
    org: String,
    bucket: String,
    token: String,
}

trait InfluxDBSettings {
    fn write_uri(self: &Self, endpoint: String) -> crate::Result<Uri>;
    fn write_uri2(self: &Self, endpoint: String) -> crate::Result<http02::Uri> {
        let uri = self.write_uri(endpoint)?;
        Ok(format!("{}", uri)
            .parse::<http02::Uri>()
            .context(super::UriParseError2)?)
    }
    fn healthcheck_uri(self: &Self, endpoint: String) -> crate::Result<Uri>;
    fn token(self: &Self) -> String;
}

impl InfluxDBSettings for InfluxDB1Settings {
    fn write_uri(self: &Self, endpoint: String) -> crate::Result<Uri> {
        encode_uri(
            &endpoint,
            "write",
            &[
                ("consistency", self.consistency.clone()),
                ("db", Some(self.database.clone())),
                ("rp", self.retention_policy_name.clone()),
                ("p", self.password.clone()),
                ("u", self.username.clone()),
                ("precision", Some("ns".to_owned())),
            ],
        )
    }

    fn healthcheck_uri(self: &Self, endpoint: String) -> crate::Result<Uri> {
        encode_uri(&endpoint, "ping", &[])
    }

    fn token(self: &Self) -> String {
        "".to_string()
    }
}

impl InfluxDBSettings for InfluxDB2Settings {
    fn write_uri(self: &Self, endpoint: String) -> crate::Result<Uri> {
        encode_uri(
            &endpoint,
            "api/v2/write",
            &[
                ("org", Some(self.org.clone())),
                ("bucket", Some(self.bucket.clone())),
                ("precision", Some("ns".to_owned())),
            ],
        )
    }

    fn healthcheck_uri(self: &Self, endpoint: String) -> crate::Result<Uri> {
        encode_uri(&endpoint, "health", &[])
    }

    fn token(self: &Self) -> String {
        self.token.clone()
    }
}

fn influxdb_settings(
    influxdb1_settings: Option<InfluxDB1Settings>,
    influxdb2_settings: Option<InfluxDB2Settings>,
) -> Result<Box<dyn InfluxDBSettings>, crate::Error> {
    if influxdb1_settings.is_some() & influxdb2_settings.is_some() {
        return Err(ConfigError::BothConfiguration {
            v1_settings: influxdb1_settings.unwrap(),
            v2_settings: influxdb2_settings.unwrap(),
        }
        .into());
    }

    if influxdb1_settings.is_none() & influxdb2_settings.is_none() {
        return Err(ConfigError::MissingConfiguration.into());
    }

    if let Some(settings) = influxdb1_settings {
        Ok(Box::new(settings))
    } else {
        Ok(Box::new(influxdb2_settings.unwrap()))
    }
}

// V1: https://docs.influxdata.com/influxdb/v1.7/tools/api/#ping-http-endpoint
// V2: https://v2.docs.influxdata.com/v2.0/api/#operation/GetHealth
fn healthcheck(
    endpoint: String,
    influxdb1_settings: Option<InfluxDB1Settings>,
    influxdb2_settings: Option<InfluxDB2Settings>,
    resolver: Resolver,
) -> crate::Result<super::Healthcheck> {
    let settings = influxdb_settings(influxdb1_settings.clone(), influxdb2_settings.clone())?;

    let endpoint = endpoint.clone();

    let uri = settings.healthcheck_uri(endpoint)?;

    let request = hyper13::Request::get(uri)
        .body(hyper13::Body::empty())
        .unwrap();

    let mut client = HttpClient::new(resolver, None)?;

    let healthcheck = client
        .call(request)
        .compat()
        .map_err(|err| err.into())
        .and_then(|response| match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::NO_CONTENT => Ok(()),
            other => Err(super::HealthcheckError::UnexpectedStatus2 { status: other }.into()),
        });

    Ok(Box::new(healthcheck))
}

// https://v2.docs.influxdata.com/v2.0/reference/syntax/line-protocol/
fn influx_line_protocol(
    measurement: String,
    metric_type: &str,
    tags: Option<BTreeMap<String, String>>,
    fields: Option<HashMap<String, Field>>,
    timestamp: i64,
    line_protocol: &mut String,
) {
    // Fields
    let unwrapped_fields = fields.unwrap_or_else(|| HashMap::new());
    // LineProtocol should have a field
    if unwrapped_fields.is_empty() {
        return;
    }

    encode_string(measurement, line_protocol);
    line_protocol.push(',');

    // Tags
    let mut unwrapped_tags = tags.unwrap_or_else(|| BTreeMap::new());
    unwrapped_tags.insert("metric_type".to_owned(), metric_type.to_owned());
    encode_tags(unwrapped_tags, line_protocol);
    line_protocol.push(' ');

    // Fields
    encode_fields(unwrapped_fields, line_protocol);
    line_protocol.push(' ');

    // Timestamp
    line_protocol.push_str(&timestamp.to_string());
    line_protocol.push('\n');
}

fn encode_tags(tags: BTreeMap<String, String>, output: &mut String) {
    let sorted = tags
        // sort by key
        .iter()
        .collect::<BTreeMap<_, _>>();

    for (key, value) in sorted {
        if key.is_empty() || value.is_empty() {
            continue;
        }
        encode_string(key.to_string(), output);
        output.push('=');
        encode_string(value.to_string(), output);
        output.push(',');
    }

    // remove last ','
    output.pop();
}

fn encode_fields(fields: HashMap<String, Field>, output: &mut String) {
    for (key, value) in fields.into_iter() {
        encode_string(key.to_string(), output);
        output.push('=');
        match value {
            Field::String(s) => {
                output.push('"');
                for c in s.chars() {
                    if "\\\"".contains(c) {
                        output.push('\\');
                    }
                    output.push(c);
                }
                output.push('"');
            }
            Field::Float(f) => output.push_str(&f.to_string()),
            Field::UnsignedInt(i) => {
                output.push_str(&i.to_string());
                output.push('u');
            }
            Field::Int(i) => {
                output.push_str(&i.to_string());
                output.push('i');
            }
            Field::Bool(b) => {
                output.push_str(&b.to_string());
            }
        };
        output.push(',');
    }

    // remove last ','
    output.pop();
}

fn encode_string(key: String, output: &mut String) {
    for c in key.chars() {
        if "\\, =".contains(c) {
            output.push('\\');
        }
        output.push(c);
    }
}

fn encode_timestamp(timestamp: Option<DateTime<Utc>>) -> i64 {
    if let Some(ts) = timestamp {
        ts.timestamp_nanos()
    } else {
        encode_timestamp(Some(Utc::now()))
    }
}

fn encode_namespace(namespace: &str, name: &str) -> String {
    if !namespace.is_empty() {
        format!("{}.{}", namespace, name)
    } else {
        name.to_string()
    }
}

fn encode_uri(endpoint: &str, path: &str, pairs: &[(&str, Option<String>)]) -> crate::Result<Uri> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());

    for pair in pairs {
        if let Some(v) = &pair.1 {
            serializer.append_pair(pair.0, v);
        }
    }

    let mut url = if endpoint.ends_with('/') {
        format!("{}{}?{}", endpoint, path, serializer.finish())
    } else {
        format!("{}/{}?{}", endpoint, path, serializer.finish())
    };

    if url.ends_with("?") {
        url.pop();
    }

    Ok(url.parse::<Uri>().context(super::UriParseError2)?)
}

#[cfg(test)]
#[allow(dead_code)]
pub mod test_util {
    use super::*;
    use chrono::offset::TimeZone;

    pub(crate) const ORG: &str = "my-org";
    pub(crate) const BUCKET: &str = "my-bucket";
    pub(crate) const TOKEN: &str = "my-token";
    pub(crate) const DATABASE: &str = "my-database";

    pub(crate) fn ts() -> DateTime<Utc> {
        Utc.ymd(2018, 11, 14).and_hms_nano(8, 9, 10, 11)
    }

    pub(crate) fn tags() -> BTreeMap<String, String> {
        vec![
            ("normal_tag".to_owned(), "value".to_owned()),
            ("true_tag".to_owned(), "true".to_owned()),
            ("empty_tag".to_owned(), "".to_owned()),
        ]
        .into_iter()
        .collect()
    }

    pub(crate) fn assert_fields(value: String, fields: Vec<&str>) {
        let encoded_fields: Vec<&str> = value.split(',').collect();

        assert_eq!(fields.len(), encoded_fields.len());

        for field in fields.into_iter() {
            assert!(
                encoded_fields.contains(&field),
                format!("Fields: {} has to have: {}", value, field)
            )
        }
    }

    // ns.requests,metric_type=distribution,normal_tag=value,true_tag=true avg=1.875,count=8,max=3,median=2,min=1,quantile_0.95=3,sum=15 1542182950000000011
    //
    // =>
    //
    // ns.requests
    // metric_type=distribution,normal_tag=value,true_tag=true
    // avg=1.875,count=8,max=3,median=2,min=1,quantile_0.95=3,sum=15
    // 1542182950000000011
    //
    pub(crate) fn split_line_protocol(line_protocol: &str) -> (&str, &str, String, &str) {
        let mut split = line_protocol.splitn(2, ',').collect::<Vec<&str>>();
        let measurement = split[0];

        split = split[1].splitn(3, ' ').collect::<Vec<&str>>();

        return (measurement, split[0], split[1].to_string(), split[2]);
    }

    pub(crate) fn onboarding_v2() {
        let mut body = std::collections::HashMap::new();
        body.insert("username", "my-user");
        body.insert("password", "my-password");
        body.insert("org", ORG);
        body.insert("bucket", BUCKET);
        body.insert("token", TOKEN);

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let res = client
            .post("http://localhost:9999/api/v2/setup")
            .json(&body)
            .header("accept", "application/json")
            .send()
            .unwrap();

        let status = res.status();

        assert!(
            status == http::StatusCode::CREATED || status == http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("UnexpectedStatus: {}", status)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::influxdb::test_util::{assert_fields, tags, ts};

    #[derive(Deserialize, Serialize, Debug, Clone, Default)]
    #[serde(deny_unknown_fields)]
    pub struct InfluxDBTestConfig {
        #[serde(flatten)]
        pub influxdb1_settings: Option<InfluxDB1Settings>,
        #[serde(flatten)]
        pub influxdb2_settings: Option<InfluxDB2Settings>,
    }

    #[test]
    fn test_influxdb_settings_both() {
        let config = r#"
        bucket = "my-bucket"
        org = "my-org"
        token = "my-token"
        database = "my-database"
    "#;
        let config: InfluxDBTestConfig = toml::from_str(&config).unwrap();
        let settings = influxdb_settings(config.influxdb1_settings, config.influxdb2_settings);
        match settings {
            Ok(_) => assert!(false, "Expected error"),
            Err(e) => assert_eq!(format!("{}",e), "Unclear settings. Both version configured v1: InfluxDB1Settings { database: \"my-database\", consistency: None, retention_policy_name: None, username: None, password: None }, v2: InfluxDB2Settings { org: \"my-org\", bucket: \"my-bucket\", token: \"my-token\" }.".to_owned())
        }
    }

    #[test]
    fn test_influxdb_settings_missing() {
        let config = r#"
    "#;
        let config: InfluxDBTestConfig = toml::from_str(&config).unwrap();
        let settings = influxdb_settings(config.influxdb1_settings, config.influxdb2_settings);
        match settings {
            Ok(_) => assert!(false, "Expected error"),
            Err(e) => assert_eq!(
                format!("{}", e),
                "InfluxDB v1 or v2 should be configured as endpoint.".to_owned()
            ),
        }
    }

    #[test]
    fn test_influxdb1_settings() {
        let config = r#"
        database = "my-database"
    "#;
        let config: InfluxDBTestConfig = toml::from_str(&config).unwrap();
        let _ = influxdb_settings(config.influxdb1_settings, config.influxdb2_settings).unwrap();
    }

    #[test]
    fn test_influxdb2_settings() {
        let config = r#"
        bucket = "my-bucket"
        org = "my-org"
        token = "my-token"
    "#;
        let config: InfluxDBTestConfig = toml::from_str(&config).unwrap();
        let _ = influxdb_settings(config.influxdb1_settings, config.influxdb2_settings).unwrap();
    }

    #[test]
    fn test_influxdb1_test_write_uri() {
        let settings = InfluxDB1Settings {
            consistency: Some("quorum".to_owned()),
            database: "vector_db".to_owned(),
            retention_policy_name: Some("autogen".to_owned()),
            username: Some("writer".to_owned()),
            password: Some("secret".to_owned()),
        };

        let uri = settings
            .write_uri("http://localhost:8086".to_owned())
            .unwrap();
        assert_eq!("http://localhost:8086/write?consistency=quorum&db=vector_db&rp=autogen&p=secret&u=writer&precision=ns", uri.to_string())
    }

    #[test]
    fn test_influxdb2_test_write_uri() {
        let settings = InfluxDB2Settings {
            org: "my-org".to_owned(),
            bucket: "my-bucket".to_owned(),
            token: "my-token".to_owned(),
        };

        let uri = settings
            .write_uri("http://localhost:9999".to_owned())
            .unwrap();
        assert_eq!(
            "http://localhost:9999/api/v2/write?org=my-org&bucket=my-bucket&precision=ns",
            uri.to_string()
        )
    }

    #[test]
    fn test_influxdb1_test_healthcheck_uri() {
        let settings = InfluxDB1Settings {
            consistency: Some("quorum".to_owned()),
            database: "vector_db".to_owned(),
            retention_policy_name: Some("autogen".to_owned()),
            username: Some("writer".to_owned()),
            password: Some("secret".to_owned()),
        };

        let uri = settings
            .healthcheck_uri("http://localhost:8086".to_owned())
            .unwrap();
        assert_eq!("http://localhost:8086/ping", uri.to_string())
    }

    #[test]
    fn test_influxdb2_test_healthcheck_uri() {
        let settings = InfluxDB2Settings {
            org: "my-org".to_owned(),
            bucket: "my-bucket".to_owned(),
            token: "my-token".to_owned(),
        };

        let uri = settings
            .healthcheck_uri("http://localhost:9999".to_owned())
            .unwrap();
        assert_eq!("http://localhost:9999/health", uri.to_string())
    }

    #[test]
    fn test_encode_tags() {
        let mut value = String::new();
        encode_tags(tags(), &mut value);

        assert_eq!(value, "normal_tag=value,true_tag=true");

        let tags_to_escape = vec![
            ("tag".to_owned(), "val=ue".to_owned()),
            ("name escape".to_owned(), "true".to_owned()),
            ("value_escape".to_owned(), "value escape".to_owned()),
            ("a_first_place".to_owned(), "10".to_owned()),
        ]
        .into_iter()
        .collect();

        let mut value = String::new();
        encode_tags(tags_to_escape, &mut value);
        assert_eq!(
            value,
            "a_first_place=10,name\\ escape=true,tag=val\\=ue,value_escape=value\\ escape"
        );
    }

    #[test]
    fn test_encode_fields() {
        let fields = vec![
            (
                "field_string".to_owned(),
                Field::String("string value".to_owned()),
            ),
            (
                "field_string_escape".to_owned(),
                Field::String("string\\val\"ue".to_owned()),
            ),
            ("field_float".to_owned(), Field::Float(123.45)),
            ("field_unsigned_int".to_owned(), Field::UnsignedInt(657)),
            ("field_int".to_owned(), Field::Int(657646)),
            ("field_bool_true".to_owned(), Field::Bool(true)),
            ("field_bool_false".to_owned(), Field::Bool(false)),
            ("escape key".to_owned(), Field::Float(10.0)),
        ]
        .into_iter()
        .collect();

        let mut value = String::new();
        encode_fields(fields, &mut value);
        assert_fields(
            value,
            [
                "escape\\ key=10",
                "field_float=123.45",
                "field_string=\"string value\"",
                "field_string_escape=\"string\\\\val\\\"ue\"",
                "field_unsigned_int=657u",
                "field_int=657646i",
                "field_bool_true=true",
                "field_bool_false=false",
            ]
            .to_vec(),
        )
    }

    #[test]
    fn test_encode_string() {
        let mut value = String::new();
        encode_string("measurement_name".to_string(), &mut value);
        assert_eq!(value, "measurement_name");

        let mut value = String::new();
        encode_string("measurement name".to_string(), &mut value);
        assert_eq!(value, "measurement\\ name");

        let mut value = String::new();
        encode_string("measurement=name".to_string(), &mut value);
        assert_eq!(value, "measurement\\=name");

        let mut value = String::new();
        encode_string("measurement,name".to_string(), &mut value);
        assert_eq!(value, "measurement\\,name");
    }

    #[test]
    fn test_encode_timestamp() {
        let start = Utc::now().timestamp_nanos();
        assert_eq!(encode_timestamp(Some(ts())), 1542182950000000011);
        assert!(encode_timestamp(None) >= start)
    }

    #[test]
    fn test_encode_namespace() {
        assert_eq!(encode_namespace("services", "status"), "services.status");
        assert_eq!(encode_namespace("", "status"), "status")
    }

    #[test]
    fn test_encode_uri_valid() {
        let uri = encode_uri(
            "http://localhost:9999",
            "api/v2/write",
            &[
                ("org", Some("my-org".to_owned())),
                ("bucket", Some("my-bucket".to_owned())),
                ("precision", Some("ns".to_owned())),
            ],
        )
        .unwrap();
        assert_eq!(
            uri,
            "http://localhost:9999/api/v2/write?org=my-org&bucket=my-bucket&precision=ns"
        );

        let uri = encode_uri(
            "http://localhost:9999/",
            "api/v2/write",
            &[
                ("org", Some("my-org".to_owned())),
                ("bucket", Some("my-bucket".to_owned())),
            ],
        )
        .unwrap();
        assert_eq!(
            uri,
            "http://localhost:9999/api/v2/write?org=my-org&bucket=my-bucket"
        );

        let uri = encode_uri(
            "http://localhost:9999",
            "api/v2/write",
            &[
                ("org", Some("Orgazniation name".to_owned())),
                ("bucket", Some("Bucket=name".to_owned())),
                ("none", None),
            ],
        )
        .unwrap();
        assert_eq!(
            uri,
            "http://localhost:9999/api/v2/write?org=Orgazniation+name&bucket=Bucket%3Dname"
        );
    }

    #[test]
    fn test_encode_uri_invalid() {
        encode_uri(
            "localhost:9999",
            "api/v2/write",
            &[
                ("org", Some("my-org".to_owned())),
                ("bucket", Some("my-bucket".to_owned())),
            ],
        )
        .unwrap_err();
    }
}

#[cfg(feature = "influxdb-integration-tests")]
#[cfg(test)]
mod integration_tests {
    use crate::sinks::influxdb::test_util::{onboarding_v2, BUCKET, DATABASE, ORG, TOKEN};
    use crate::sinks::influxdb::{healthcheck, InfluxDB1Settings, InfluxDB2Settings};
    use crate::test_util::runtime;
    use crate::topology::SinkContext;

    #[test]
    fn influxdb2_healthchecks_ok() {
        onboarding_v2();

        let mut rt = runtime();
        let cx = SinkContext::new_test(rt.executor());
        let endpoint = "http://localhost:9999".to_string();
        let influxdb1_settings = None;
        let influxdb2_settings = Some(InfluxDB2Settings {
            org: ORG.to_string(),
            bucket: BUCKET.to_string(),
            token: TOKEN.to_string(),
        });

        let healthcheck = healthcheck(
            endpoint,
            influxdb1_settings,
            influxdb2_settings,
            cx.resolver(),
        )
        .unwrap();
        rt.block_on(healthcheck).unwrap();
    }

    #[test]
    fn influxdb2_healthchecks_fail() {
        onboarding_v2();

        let mut rt = runtime();
        let cx = SinkContext::new_test(rt.executor());
        let endpoint = "http://not_exist:9999".to_string();
        let influxdb1_settings = None;
        let influxdb2_settings = Some(InfluxDB2Settings {
            org: ORG.to_string(),
            bucket: BUCKET.to_string(),
            token: TOKEN.to_string(),
        });
        let healthcheck = healthcheck(
            endpoint,
            influxdb1_settings,
            influxdb2_settings,
            cx.resolver(),
        )
        .unwrap();
        rt.block_on(healthcheck).unwrap_err();
    }

    #[test]
    fn influxdb1_healthchecks_ok() {
        let mut rt = runtime();
        let cx = SinkContext::new_test(rt.executor());
        let endpoint = "http://localhost:8086".to_string();
        let influxdb1_settings = Some(InfluxDB1Settings {
            database: DATABASE.to_string(),
            consistency: None,
            retention_policy_name: None,
            username: None,
            password: None,
        });
        let influxdb2_settings = None;

        let healthcheck = healthcheck(
            endpoint,
            influxdb1_settings,
            influxdb2_settings,
            cx.resolver(),
        )
        .unwrap();
        rt.block_on(healthcheck).unwrap();
    }

    #[test]
    fn influxdb1_healthchecks_fail() {
        let mut rt = runtime();
        let cx = SinkContext::new_test(rt.executor());
        let endpoint = "http://not_exist:8086".to_string();
        let influxdb1_settings = Some(InfluxDB1Settings {
            database: DATABASE.to_string(),
            consistency: None,
            retention_policy_name: None,
            username: None,
            password: None,
        });
        let influxdb2_settings = None;

        let healthcheck = healthcheck(
            endpoint,
            influxdb1_settings,
            influxdb2_settings,
            cx.resolver(),
        )
        .unwrap();
        rt.block_on(healthcheck).unwrap_err();
    }
}
