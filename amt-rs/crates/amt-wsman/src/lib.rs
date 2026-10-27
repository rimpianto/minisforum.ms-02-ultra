mod boot;
mod client;
mod envelope;
mod cim;
mod config;
mod error;

pub use boot::{bios_boot_pending, set_bios_setup};
pub use cim::PowerState;
pub use error::AmtWsManError;

use anyhow::{anyhow, Context, Result};
use uuid::Uuid;
use reqwest::{header, Client, Url};
use std::time::Duration;

// Import the local config struct defined in this crate.
pub use crate::config::AmtConfig;

/// Asynchronously set the power state of a remote AMT device.
///
/// * `cfg` – configuration containing host, port, username and password.
/// * `state` – desired power state (enum maps to the numeric value required by the
///   WS‑Man SOAP envelope).
///
/// The function performs a two‑step HTTP‑Digest handshake exactly like the
/// JavaScript implementation in MeshCommander.
pub async fn set_power(cfg: &AmtConfig, state: PowerState) -> Result<()> {
    // ---------------------------------------------------------------------
    // 1️⃣ Build the target URL.
    // ---------------------------------------------------------------------
    let url = format!("https://{}:{}/wsman", cfg.host, cfg.port);
    let url = Url::parse(&url).context("Invalid WS‑Man URL")?;

    // ---------------------------------------------------------------------
    // 2️⃣ Build the SOAP envelope (same XML MeshCommander sends).
    // ---------------------------------------------------------------------
    let msg_id = Uuid::new_v4().to_string();
    let envelope = format!(r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><s:Header><a:Action s:mustUnderstand="true">http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService/RequestPowerStateChange</a:Action><a:ReplyTo><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address></a:ReplyTo><a:MessageID>uuid:{}</a:MessageID><a:To s:mustUnderstand="true">https://{}:{}/wsman</a:To><w:ResourceURI s:mustUnderstand="true">http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService</w:ResourceURI><w:SelectorSet><w:Selector Name="CreationClassName">CIM_PowerManagementService</w:Selector><w:Selector Name="Name">Intel(r) AMT Power Management Service</w:Selector><w:Selector Name="SystemCreationClassName">CIM_ComputerSystem</w:Selector><w:Selector Name="SystemName">Intel(r) AMT</w:Selector></w:SelectorSet></s:Header><s:Body><p:RequestPowerStateChange_INPUT xmlns:p="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService"><p:PowerState>{}</p:PowerState><p:ManagedElement xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address><a:ReferenceParameters><w:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ComputerSystem</w:ResourceURI><w:SelectorSet><w:Selector Name="CreationClassName">CIM_ComputerSystem</w:Selector><w:Selector Name="Name">ManagedSystem</w:Selector></w:SelectorSet></a:ReferenceParameters></p:ManagedElement></p:RequestPowerStateChange_INPUT></s:Body></s:Envelope>"#, msg_id, cfg.host, cfg.port, state as u8);
    if std::env::var("AMT_DEBUG_XML").is_ok() {
        println!("--- SOAP REQUEST START ---\n{}\n--- SOAP REQUEST END ---", envelope);
    }

    // ---------------------------------------------------------------------
    // 3️⃣ Create a reqwest client that accepts self‑signed certificates.
    // ---------------------------------------------------------------------
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;

    // ---------------------------------------------------------------------
    // 4️⃣ First request – expect a 401 with WWW‑Authenticate header.
    // ---------------------------------------------------------------------
    let first = client
        .post(url.clone())
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .body(envelope.clone())
        .send()
        .await
        .context("Initial WS‑Man request failed")?;

    if first.status().is_success() {
        // Some firmware allows unauthenticated power change – treat as success.
        return Ok(());
    }

    // ---------------------------------------------------------------------
    // 5️⃣ Extract the Digest challenge and build the Authorization header.
    // ---------------------------------------------------------------------
    let www_auth = first
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .ok_or_else(|| anyhow!("WWW-Authenticate header missing"))?
        .to_str()
        .context("Invalid WWW-Authenticate header")?;

    // Parse the challenge using http_auth crate and build a PasswordClient.
    let mut pw_client = http_auth::PasswordClient::try_from(www_auth)
        .map_err(|e| anyhow!("Failed to parse WWW-Authenticate header for Digest auth: {}", e))?;
    // Build the parameters required by the client.
    let auth_header = pw_client
        .respond(&http_auth::PasswordParams {
            username: &cfg.username,
            password: &cfg.password,
            uri: url.path(),
            method: "POST",
            body: None,
        })
        .map_err(|e| anyhow!("Digest auth response error: {}", e))?;

    // ---------------------------------------------------------------------
    // 6️⃣ Authenticated request.
    // ---------------------------------------------------------------------
    let final_resp = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .header(header::AUTHORIZATION, auth_header)
        .body(envelope)
        .send()
        .await
        .context("Authenticated WS‑Man request failed")?;

    // ---------------------------------------------------------------
    // 7️⃣ Verify the WS‑Man response body contains a successful ReturnValue.
    // ---------------------------------------------------------------
    let status = final_resp.status();
    let body = final_resp.text().await.unwrap_or_default();
    if status.is_success() {
        // Look for <g:ReturnValue>0</g:ReturnValue>
        let ret_val = {
            let start = body.find("<g:ReturnValue>");
            let end = body.find("</g:ReturnValue>");
            if let (Some(s), Some(e)) = (start, end) {
                body[s + "<g:ReturnValue>".len()..e].trim()
            } else {
                ""
            }
        };
        if ret_val == "0" {
            Ok(())
        } else {
            Err(anyhow!(
                "WS‑Man request succeeded but ReturnValue is {} – body: {}",
                ret_val,
                body
            ))
        }
    } else {
        Err(anyhow!("WS‑Man request returned {} – {}", status, body))
    }
}

/// Retrieve current PowerState and RequestedPowerState via a WS‑Man GET request.
pub async fn get_power_state(cfg: &AmtConfig) -> Result<(u8, u8)> {
    // Build URL.
    let url = format!("https://{}:{}/wsman", cfg.host, cfg.port);
    let url = Url::parse(&url).context("Invalid WS‑Man URL")?;

    // Build SOAP envelope for Get action.
    let msg_id = Uuid::new_v4().to_string();
    // Single-line body: AMT rejects envelopes with whitespace or line breaks
    // in some firmware revisions, and the one-line form is the one verified
    // against this device.
    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><s:Header><a:Action s:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2004/09/transfer/Get</a:Action><a:ReplyTo><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address></a:ReplyTo><a:MessageID>uuid:{}</a:MessageID><a:To s:mustUnderstand="true">https://{}:{}/wsman</a:To><w:ResourceURI s:mustUnderstand="true">http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_AssociatedPowerManagementService</w:ResourceURI></s:Header><s:Body/></s:Envelope>"#, msg_id, cfg.host, cfg.port
    );

    if std::env::var("AMT_DEBUG_XML").is_ok() {
        eprintln!("--- SOAP GET REQUEST START ---\n{}\n--- SOAP GET REQUEST END ---", envelope);
    }

    // HTTP client, accept self‑signed certs.
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;

    // First request – expect 401.
    let first = client
        .post(url.clone())
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .body(envelope.clone())
        .send()
        .await
        .context("Initial WS‑Man request failed")?;

    if first.status().is_success() {
        let body = first.text().await.unwrap_or_default();
        return parse_power_response(&body);
    }

    // Digest auth.
    let www_auth = first
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .ok_or_else(|| anyhow!("WWW-Authenticate header missing"))?
        .to_str()
        .context("Invalid WWW-Authenticate header")?;
    let mut pw_client = http_auth::PasswordClient::try_from(www_auth)
        .map_err(|e| anyhow!("Failed to parse WWW-Authenticate header for Digest auth: {}", e))?;
    let auth_header = pw_client
        .respond(&http_auth::PasswordParams {
            username: &cfg.username,
            password: &cfg.password,
            uri: url.path(),
            method: "POST",
            body: None,
        })
        .map_err(|e| anyhow!("Digest auth response error: {}", e))?;

    // Authenticated request.
    let final_resp = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=UTF-8")
        .header(header::AUTHORIZATION, auth_header)
        .body(envelope)
        .send()
        .await
        .context("Authenticated WS‑Man request failed")?;

    let status = final_resp.status();
    let body = final_resp.text().await.unwrap_or_default();
    if status.is_success() {
        parse_power_response(&body)
    } else {
        Err(anyhow!("WS‑Man request returned {} – {}", status, body))
    }
}

// Helper to extract PowerState and RequestedPowerState from a WS‑Man response body.
fn parse_power_response(body: &str) -> Result<(u8, u8)> {
    let power_state = {
        let start = body.find("<g:PowerState>");
        let end = body.find("</g:PowerState>");
        if let (Some(s), Some(e)) = (start, end) {
            body[s + "<g:PowerState>".len()..e]
                .trim()
                .parse::<u8>()
                .unwrap_or(0)
        } else {
            0
        }
    };
    let requested = {
        let start = body.find("<g:RequestedPowerState>");
        let end = body.find("</g:RequestedPowerState>");
        if let (Some(s), Some(e)) = (start, end) {
            body[s + "<g:RequestedPowerState>".len()..e]
                .trim()
                .parse::<u8>()
                .unwrap_or(0)
        } else {
            0
        }
    };
    Ok((power_state, requested))
}
