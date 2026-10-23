// Build SOAP envelope strings
pub fn envelope_body(action: &str, body: &str) -> String {
    format!(
        "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header/><Body>{}{}{}</Body></Envelope>",
        "<Action>", action, body
    )
}

/// Build the SOAP request for a power state change.
/// `state` is converted to its numeric value as required by the AMT WS‑Man API.
pub fn request_power_state_change(state: super::PowerState) -> String {
    let action = "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService/RequestPowerStateChange";
    let body = format!("<h:PowerState>{}</h:PowerState>", state as u8);
    envelope_body(action, &body)
}

