//! The services a tenant can book, each with its read-only tools. A service
//! whose environment is not set (not booked) has no endpoint.

pub mod docs;
pub mod flights;
pub mod photos;
pub mod places;

use reqwest::Client;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::limits::{ToolError, Zone};
use crate::members::Member;
use crate::schema::{Args, Tool};

/// What a tool call runs with.
pub struct Ctx<'a> {
    pub member: &'a Member,
    /// e.g. https://muster.castorcloud.ch; results link back into the portal.
    pub portal_origin: &'a str,
    pub http: &'a Client,
    pub zone: &'a Zone,
}

/// A display string from an upstream answer; anything but a string counts
/// as absent instead of failing the whole answer.
pub fn lenient_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        _ => None,
    })
}

/// Same keys as the portal and the provisioner use for booked services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Photos,
    Docs,
    Places,
    Flights,
}

impl Key {
    pub const ALL: [Key; 4] = [Key::Photos, Key::Docs, Key::Places, Key::Flights];

    pub fn as_str(self) -> &'static str {
        match self {
            Key::Photos => "photos",
            Key::Docs => "docs",
            Key::Places => "places",
            Key::Flights => "flights",
        }
    }

    pub fn parse(s: &str) -> Option<Key> {
        Key::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Default)]
pub struct Services {
    pub photos: Option<photos::Photos>,
    pub docs: Option<docs::Docs>,
    pub places: Option<places::Places>,
    pub flights: Option<flights::Flights>,
}

impl Services {
    /// From the environment the tenant compose file gives the container.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let set = |k: &str| env(k).filter(|v| !v.is_empty());
        Self {
            photos: set("IMMICH_URL")
                .zip(set("IMMICH_DB_URL"))
                .map(|(u, d)| photos::Photos::new(u, d)),
            docs: match (
                set("PAPERLESS_URL"),
                set("PAPERLESS_USER"),
                set("PAPERLESS_PASSWORD"),
            ) {
                (Some(u), Some(user), Some(pw)) => Some(docs::Docs::new(u, user, pw)),
                _ => None,
            },
            places: set("DAWARICH_URL")
                .zip(set("DAWARICH_DB_URL"))
                .map(|(u, d)| places::Places::new(u, d)),
            flights: set("AIRTRAIL_URL")
                .zip(set("AIRTRAIL_DB_URL"))
                .map(|(u, d)| flights::Flights::new(u, d)),
        }
    }

    pub fn configured(&self) -> Vec<Key> {
        Key::ALL
            .into_iter()
            .filter(|k| self.tools(*k).is_some())
            .collect()
    }

    /// The tools of a booked service.
    pub fn tools(&self, key: Key) -> Option<Vec<Tool>> {
        match key {
            Key::Photos => self.photos.as_ref().map(|_| photos::Photos::tools()),
            Key::Docs => self.docs.as_ref().map(|_| docs::Docs::tools()),
            Key::Places => self.places.as_ref().map(|_| places::Places::tools()),
            Key::Flights => self.flights.as_ref().map(|_| flights::Flights::tools()),
        }
    }

    /// Runs one of the service's tools (`name` is one of `tools(key)`).
    pub async fn call(
        &self,
        key: Key,
        ctx: &Ctx<'_>,
        name: &str,
        args: &Args,
    ) -> Result<Value, ToolError> {
        let not_booked = || ToolError::internal("service not booked");
        match key {
            Key::Photos => {
                self.photos
                    .as_ref()
                    .ok_or_else(not_booked)?
                    .call(ctx, name, args)
                    .await
            }
            Key::Docs => {
                self.docs
                    .as_ref()
                    .ok_or_else(not_booked)?
                    .call(ctx, name, args)
                    .await
            }
            Key::Places => {
                self.places
                    .as_ref()
                    .ok_or_else(not_booked)?
                    .call(ctx, name, args)
                    .await
            }
            Key::Flights => {
                self.flights
                    .as_ref()
                    .ok_or_else(not_booked)?
                    .call(ctx, name, args)
                    .await
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub mod tests {
    use super::*;
    use crate::members::tests::member;

    pub fn with_ctx<R>(portal_origin: &str, f: impl FnOnce(&Ctx) -> R) -> R {
        let member = member("anna", "token-a");
        let http = Client::new();
        let zone = Zone::new("Europe/Zurich");
        f(&Ctx {
            member: &member,
            portal_origin,
            http: &http,
            zone: &zone,
        })
    }

    #[test]
    fn only_booked_services_are_served() {
        let env = |k: &str| match k {
            "IMMICH_URL" => Some("http://immich:2283".to_owned()),
            "IMMICH_DB_URL" => Some("postgres://x@db/immich".to_owned()),
            "PAPERLESS_URL" => Some("http://paperless:8000".to_owned()),
            "PAPERLESS_USER" => Some("pc-service".to_owned()),
            "PAPERLESS_PASSWORD" => Some(String::new()),
            "AIRTRAIL_URL" => Some("http://airtrail:3000".to_owned()),
            "AIRTRAIL_DB_URL" => Some("postgres://x@db/airtrail".to_owned()),
            _ => None,
        };
        let s = Services::from_env(env);
        assert_eq!(s.configured(), vec![Key::Photos, Key::Flights]);
        assert!(Services::from_env(|_| None).configured().is_empty());
        assert_eq!(Key::parse("docs"), Some(Key::Docs));
        assert_eq!(Key::parse("music"), None);
    }
}
