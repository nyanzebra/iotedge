/*
 * IoT Edge Module Workload API
 *
 * No description provided (generated by Swagger Codegen https://github.com/swagger-api/swagger-codegen)
 *
 * OpenAPI spec version: 2018-06-28
 *
 * Generated by: https://github.com/swagger-api/swagger-codegen.git
 */

use serde_derive::{Deserialize, Serialize};
#[allow(unused_imports)]
use serde_json::Value;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IdentityCertificateRequest {
    /// Certificate expiration date-time (ISO 8601)
    #[serde(rename = "expiration", skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
}

impl IdentityCertificateRequest {
    pub fn new() -> IdentityCertificateRequest {
        IdentityCertificateRequest { expiration: None }
    }

    pub fn set_expiration(&mut self, expiration: String) {
        self.expiration = Some(expiration);
    }

    pub fn with_expiration(mut self, expiration: String) -> IdentityCertificateRequest {
        self.expiration = Some(expiration);
        self
    }

    pub fn expiration(&self) -> Option<&str> {
        self.expiration.as_ref().map(AsRef::as_ref)
    }

    pub fn reset_expiration(&mut self) {
        self.expiration = None;
    }
}
