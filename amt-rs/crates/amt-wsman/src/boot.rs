//! Boot-to-BIOS: ask the firmware to stop in setup on the next boot, so nobody
//! has to hit Delete in a window of a few hundred milliseconds.

use anyhow::{anyhow, Context, Result};
use reqwest::{header, Client, Url};
use std::time::Duration;
use uuid::Uuid;

use crate::config::AmtConfig;

const NS_AMT: &str = "http://intel.com/wbem/wscim/1/amt-schema/1";

/// Send one WS-Man request, doing the Digest handshake the device insists on.
async fn call(
    cfg: &AmtConfig,
    action: &str,
    resource_uri: &str,
    selectors: &str,
    body: &str,
) -> Result<String> {
    let url = Url::parse(&format!("https://{}:{}/wsman", cfg.host, cfg.port))
        .context("invalid WS-Man URL")?;
    let msg_id = Uuid::new_v4().to_string();

    // One line: this firmware rejects envelopes with whitespace inside.
    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><s:Header><a:Action s:mustUnderstand="true">{action}</a:Action><a:ReplyTo><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address></a:ReplyTo><a:MessageID>uuid:{msg_id}</a:MessageID><a:To s:mustUnderstand="true">https://{host}:{port}/wsman</a:To><w:ResourceURI s:mustUnderstand="true">{resource_uri}</w:ResourceURI><w:SelectorSet>{selectors}</w:SelectorSet></s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
        host = cfg.host,
        port = cfg.port
    );

    if std::env::var("AMT_DEBUG_XML").is_ok() {
        eprintln!("--- SOAP REQUEST ---\n{envelope}\n--- END ---");
    }

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build HTTP client")?;

    let first = client
        .post(url.clone())
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .body(envelope.clone())
        .send()
        .await
        .context("WS-Man request failed")?;

    if first.status().is_success() {
        return Ok(first.text().await.unwrap_or_default());
    }

    let www_auth = first
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .ok_or_else(|| anyhow!("WWW-Authenticate missing"))?
        .to_str()
        .context("bad WWW-Authenticate header")?
        .to_string();

    let mut pw = http_auth::PasswordClient::try_from(www_auth.as_str())
        .map_err(|e| anyhow!("cannot parse Digest challenge: {e}"))?;
    let auth_header = pw
        .respond(&http_auth::PasswordParams {
            username: &cfg.username,
            password: &cfg.password,
            uri: url.path(),
            method: "POST",
            body: None,
        })
        .map_err(|e| anyhow!("digest auth error: {e}"))?;

    let second = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .header(header::AUTHORIZATION, auth_header)
        .body(envelope)
        .send()
        .await
        .context("authenticated WS-Man request failed")?;

    let status = second.status();
    let text = second.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("WS-Man returned {status} - {text}"));
    }
    Ok(text)
}

/// AMT_BootSettingData is a singleton: it is addressed without selectors.
fn selector_set(_name: &str, _value: &str) -> String {
    String::new()
}

/// True if the device considers the next boot a BIOS-setup boot.
pub async fn bios_boot_pending(cfg: &AmtConfig) -> Result<bool> {
    let xml = call(
        cfg,
        "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get",
        &format!("{NS_AMT}/AMT_BootSettingData"),
        "",
        "",
    )
    .await?;
    Ok(field(&xml, "BIOSSetup").map(|v| v == "true" || v == "1").unwrap_or(false))
}

/// Ask for the BIOS setup screen on the next boot (or clear the request).
/// Reversible: the firmware clears the flag once it has been used.
pub async fn set_bios_setup(cfg: &AmtConfig, enable: bool) -> Result<()> {
    let xml = call(
        cfg,
        "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get",
        &format!("{NS_AMT}/AMT_BootSettingData"),
        "",
        "",
    )
    .await?;

    if std::env::var("AMT_DEBUG_XML").is_ok() {
        eprintln!("--- GET REPLY ---\n{xml}\n--- END ---");
    }

    let start = xml
        .find("<g:AMT_BootSettingData")
        .or_else(|| xml.find(":AMT_BootSettingData"))
        .ok_or_else(|| anyhow!("AMT_BootSettingData not found in the reply"))?;
    let open_end = xml[start..]
        .find('>')
        .map(|i| start + i + 1)
        .ok_or_else(|| anyhow!("malformed instance"))?;
    let close = xml
        .rfind("</g:AMT_BootSettingData>")
        .or_else(|| xml.rfind(":AMT_BootSettingData>"))
        .ok_or_else(|| anyhow!("instance end tag not found"))?;
    let inner = &xml[open_end..close];

    // Properties the device refuses to have written back, plus the ones that
    // describe state rather than configuration.
    let read_only = [
        "BIOSLastStatus",
        "BootguardStatus",
        "OptionsCleared",
        "WinREBootEnabled",
        "UEFILocalPBABootEnabled",
        "UEFIHTTPSBootEnabled",
        "SecureBootControlEnabled",
        "UefiBootParametersArray",
        "RPEEnabled",
    ];

    let mut out = String::new();
    for element in split_elements(inner) {
        let name = element_name(&element);
        if read_only.contains(&name.as_str()) {
            continue;
        }
        if name == "BIOSSetup" {
            continue; // re-added below with the value we want
        }
        out.push_str(&element);
    }
    out.push_str(if enable {
        "<g:BIOSSetup>true</g:BIOSSetup>"
    } else {
        "<g:BIOSSetup>false</g:BIOSSetup>"
    });

    let body = format!(
        r#"<g:AMT_BootSettingData xmlns:g="{NS_AMT}/AMT_BootSettingData">{out}</g:AMT_BootSettingData>"#
    );

    let reply = call(
        cfg,
        "http://schemas.xmlsoap.org/ws/2004/09/transfer/Put",
        &format!("{NS_AMT}/AMT_BootSettingData"),
        "",
        &body,
    )
    .await?;

    if std::env::var("AMT_DEBUG_XML").is_ok() {
        eprintln!("--- PUT REPLY ---\n{reply}\n--- END ---");
    }
    Ok(())
}

/// Split a run of XML child elements, keeping attributes and whitespace.
fn split_elements(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    loop {
        let Some(open) = rest.find('<') else { break };
        let Some(gt) = rest[open..].find('>') else { break };
        let head_end = open + gt + 1;
        let head = &rest[open..head_end];
        if head.starts_with("</") {
            rest = &rest[head_end..];
            continue;
        }
        // Keep the prefix the device used: the closing tag has to match it
        // exactly, otherwise nothing is ever extracted.
        let raw_name = head
            .trim_start_matches('<')
            .split(|c: char| c == '>' || c == ' ' || c == '/' || c == '\n' || c == '\r' || c == '\t')
            .next()
            .unwrap_or("")
            .to_string();
        let close_tag = format!("</{raw_name}>");
        if let Some(close) = rest[head_end..].find(&close_tag) {
            let end = head_end + close + close_tag.len();
            out.push(rest[open..end].to_string());
            rest = &rest[end..];
        } else if head.ends_with("/>") {
            out.push(head.to_string());
            rest = &rest[head_end..];
        } else {
            break;
        }
    }
    out
}

fn element_name(element: &str) -> String {
    let e = element.trim_start_matches('<');
    let e = e.trim_start_matches(|c: char| c == '?' || c == '/');
    e.split(|c: char| c == '>' || c == ' ' || c == '/' || c == '\n' || c == '\r' || c == '\t')
        .next()
        .unwrap_or("")
        .rsplit(':')
        .next()
        .unwrap_or("")
        .to_string()
}

fn field(xml: &str, name: &str) -> Option<String> {
    let open = format!("<g:{name}>");
    let close = format!("</g:{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}
