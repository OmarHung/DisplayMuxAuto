//! Sends a prepared diagnostic report to Sentry as one event with the report
//! attached.
//!
//! Written against Sentry's envelope endpoint directly instead of the Sentry
//! SDK: a report is sent at most a few times a day and only with consent, so
//! none of the SDK's automatic capture is wanted, and the HTTP client is
//! already in the build for the updater.
//!
//! Where reports go is fixed when the app is built, from `MUXSU_SENTRY_DSN`.
//! A build without it cannot send at all and offers saving the report instead.

use std::time::Duration;

use serde_json::json;

use crate::diagnostics::{PreparedReport, ReportTrigger};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(20);

/// Where and as whom a report is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentryTarget {
    pub envelope_url: String,
    pub public_key: String,
    pub dsn: String,
}

/// The target compiled into this build, if any.
pub fn configured_target() -> Option<SentryTarget> {
    option_env!("MUXSU_SENTRY_DSN").and_then(parse_dsn)
}

/// Reads `https://PUBLIC_KEY@HOST[:PORT]/[PATH/]PROJECT_ID`.
pub fn parse_dsn(dsn: &str) -> Option<SentryTarget> {
    let dsn = dsn.trim();
    let (scheme, rest) = dsn.split_once("://")?;
    // The report describes this computer and its displays, so it never leaves
    // unencrypted, even if a build is given a plain-HTTP DSN.
    if scheme != "https" {
        return None;
    }
    let (public_key, location) = rest.split_once('@')?;
    let public_key = public_key.split(':').next()?;
    let (host, path) = location.split_once('/')?;
    let (prefix, project) = match path.trim_end_matches('/').rsplit_once('/') {
        Some((prefix, project)) => (format!("/{prefix}"), project),
        None => (String::new(), path.trim_end_matches('/')),
    };
    let valid_project = !project.is_empty() && project.chars().all(|char| char.is_ascii_digit());
    if public_key.is_empty() || host.is_empty() || !valid_project {
        return None;
    }
    Some(SentryTarget {
        envelope_url: format!("{scheme}://{host}{prefix}/api/{project}/envelope/"),
        public_key: public_key.to_owned(),
        dsn: dsn.to_owned(),
    })
}

/// One envelope: a header, the event, and the report as a JSON attachment.
pub fn envelope(target: &SentryTarget, report: &PreparedReport, sent_at_ms: u64) -> Vec<u8> {
    let (level, message, trigger) = match &report.trigger {
        ReportTrigger::Manual => ("info", "Diagnostic report", "manual"),
        ReportTrigger::SwitchFailed { .. } => ("error", "Switch failed", "switchFailed"),
    };
    let header = json!({
        "event_id": report.report_id,
        "dsn": target.dsn,
    });
    let event = json!({
        "event_id": report.report_id,
        "timestamp": sent_at_ms as f64 / 1000.0,
        "platform": "native",
        "level": level,
        "logger": "muxsu.diagnostics",
        "release": concat!("muxsu@", env!("CARGO_PKG_VERSION")),
        "message": { "formatted": message },
        "tags": { "trigger": trigger, "os": std::env::consts::OS },
        // No user context at all, so nothing is inferred from the request.
        "user": { "ip_address": null },
    });
    let event = event.to_string();
    let attachment = report.json.as_bytes();
    let mut body = Vec::with_capacity(event.len() + attachment.len() + 512);
    for line in [
        header.to_string(),
        json!({ "type": "event", "length": event.len() }).to_string(),
        event,
        json!({
            "type": "attachment",
            "length": attachment.len(),
            "filename": format!("muxsu-report-{}.json", report.report_id),
            "content_type": "application/json",
        })
        .to_string(),
    ] {
        body.extend_from_slice(line.as_bytes());
        body.push(b'\n');
    }
    body.extend_from_slice(attachment);
    body.push(b'\n');
    body
}

fn auth_header(target: &SentryTarget) -> String {
    format!(
        "Sentry sentry_version=7, sentry_key={}, sentry_client=muxsu/{}",
        target.public_key,
        env!("CARGO_PKG_VERSION")
    )
}

pub async fn upload(
    target: &SentryTarget,
    report: &PreparedReport,
    sent_at_ms: u64,
) -> Result<(), String> {
    let response = reqwest::Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?
        .post(&target.envelope_url)
        .header("X-Sentry-Auth", auth_header(target))
        .header("Content-Type", "application/x-sentry-envelope")
        .body(envelope(target, report, sent_at_ms))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", response.status()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hosted_dsn_points_at_its_projects_envelope_endpoint() {
        let target = parse_dsn("https://abc123@o42.ingest.us.sentry.io/4507").expect("valid DSN");

        assert_eq!(
            target.envelope_url,
            "https://o42.ingest.us.sentry.io/api/4507/envelope/"
        );
        assert_eq!(target.public_key, "abc123");
    }

    #[test]
    fn a_self_hosted_dsn_keeps_its_path_prefix() {
        let target = parse_dsn("https://key@sentry.example.com:9000/tools/7").expect("valid DSN");

        assert_eq!(
            target.envelope_url,
            "https://sentry.example.com:9000/tools/api/7/envelope/"
        );
    }

    #[test]
    fn malformed_dsns_are_refused() {
        for dsn in [
            "",
            "not a dsn",
            "https://o42.ingest.sentry.io/4507",
            "https://key@host/",
            "https://key@host/project",
            "ftp://key@host/1",
            // A report is sent only encrypted, however the build was configured.
            "http://key@sentry.example.com/1",
        ] {
            assert_eq!(parse_dsn(dsn), None, "{dsn}");
        }
    }

    #[test]
    fn the_envelope_carries_the_event_and_the_report_as_shown() {
        let target = parse_dsn("https://abc@sentry.example.com/1").unwrap();
        let report = PreparedReport {
            report_id: "0123456789abcdef0123456789abcdef".to_owned(),
            trigger: ReportTrigger::SwitchFailed {
                message: "ignored here".to_owned(),
            },
            json: "{\n  \"reportId\": \"x\"\n}".to_owned(),
        };

        let body = String::from_utf8(envelope(&target, &report, 1_000)).unwrap();
        let lines = body.lines().collect::<Vec<_>>();
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let event_item: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let event: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        let attachment_item: serde_json::Value = serde_json::from_str(lines[3]).unwrap();

        assert_eq!(header["event_id"], report.report_id);
        assert_eq!(event_item["length"], lines[2].len());
        assert_eq!(event["level"], "error");
        assert_eq!(event["tags"]["trigger"], "switchFailed");
        assert_eq!(attachment_item["type"], "attachment");
        assert_eq!(attachment_item["length"], report.json.len());
        assert!(body.ends_with(&format!("{}\n", report.json)));
        assert!(
            !body.contains("ignored here"),
            "the message stays in the attachment"
        );
    }
}
