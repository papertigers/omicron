// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for user-provided RSS configuration options.

use crate::bootstrap_addrs::BootstrapPeers;
use crate::http_entrypoints::BootstrapSledDescription;
use crate::http_entrypoints::CertificateUploadResponse;
use crate::http_entrypoints::CurrentRssUserConfig;
use crate::http_entrypoints::CurrentRssUserConfigInsensitive;
use crate::http_entrypoints::CurrentRssUserConfigSensitive;
use crate::RackV1Inventory;
use anyhow::bail;
use anyhow::Result;
use bootstrap_agent_client::types::BootstrapAddressDiscovery;
use bootstrap_agent_client::types::Certificate;
use bootstrap_agent_client::types::Name;
use bootstrap_agent_client::types::RackInitializeRequest;
use bootstrap_agent_client::types::RecoverySiloConfig;
use bootstrap_agent_client::types::UserId;
use gateway_client::types::SpType;
use omicron_certificates::CertificateValidator;
use omicron_common::address;
use omicron_common::api::internal::shared::RackNetworkConfig;
use sled_hardware::Baseboard;
use std::collections::BTreeSet;
use std::net::Ipv6Addr;
use wicket_common::rack_setup::PutRssUserConfigInsensitive;

// TODO-correctness For now, we always use the same rack subnet when running
// RSS. When we get to multirack, this will be wrong, but there are many other
// RSS-related things that need to change then too.
const RACK_SUBNET: Ipv6Addr =
    Ipv6Addr::new(0xfd00, 0x1122, 0x3344, 0x0100, 0, 0, 0, 0);

const RECOVERY_SILO_NAME: &str = "recovery";
const RECOVERY_SILO_USERNAME: &str = "recovery";

#[derive(Default)]
struct PartialCertificate {
    cert: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
}

/// An analogue to `RackInitializeRequest`, but with optional fields to allow
/// the user to fill it in piecemeal.
#[derive(Default)]
pub(crate) struct CurrentRssConfig {
    inventory: BTreeSet<BootstrapSledDescription>,

    bootstrap_sleds: BTreeSet<BootstrapSledDescription>,
    ntp_servers: Vec<String>,
    dns_servers: Vec<String>,
    internal_services_ip_pool_ranges: Vec<address::IpRange>,
    external_dns_zone_name: String,
    external_certificates: Vec<Certificate>,
    recovery_silo_password_hash: Option<omicron_passwords::NewPasswordHash>,
    rack_network_config: Option<RackNetworkConfig>,

    // External certificates are uploaded in two separate actions (cert then
    // key, or vice versa). Here we store a partial certificate; once we have
    // both parts, we validate it and promote it to be a member of
    // external_certificates.
    partial_external_certificate: PartialCertificate,
}

impl CurrentRssConfig {
    pub(crate) fn populate_available_bootstrap_sleds_from_inventory(
        &mut self,
        inventory: &RackV1Inventory,
        bootstrap_peers: &BootstrapPeers,
    ) {
        let bootstrap_sleds = bootstrap_peers.sleds();

        self.inventory = inventory
            .sps
            .iter()
            .filter_map(|sp| {
                if sp.id.type_ != SpType::Sled {
                    return None;
                }
                let state = sp.state.as_ref()?;
                let baseboard = Baseboard::new_gimlet(
                    state.serial_number.clone(),
                    state.model.clone(),
                    state.revision.into(),
                );
                let bootstrap_ip = bootstrap_sleds.get(&baseboard).copied();
                Some(BootstrapSledDescription {
                    id: sp.id,
                    baseboard,
                    bootstrap_ip,
                })
            })
            .collect();
    }

    pub(crate) fn start_rss_request(
        &self,
        bootstrap_peers: &BootstrapPeers,
    ) -> Result<RackInitializeRequest> {
        // Basic "client-side" checks.
        if self.bootstrap_sleds.is_empty() {
            bail!("bootstrap_sleds is empty (have you uploaded a config?)");
        }
        if self.ntp_servers.is_empty() {
            bail!("at least one NTP server is required");
        }
        if self.dns_servers.is_empty() {
            bail!("at least one DNS server is required");
        }
        if self.internal_services_ip_pool_ranges.is_empty() {
            bail!("at least one internal services IP pool range is required");
        }
        if self.external_dns_zone_name.is_empty() {
            bail!("external dns zone name is required");
        }
        if self.external_certificates.is_empty() {
            bail!("at least one certificate/key pair is required");
        }
        let Some(recovery_silo_password_hash)
            = self.recovery_silo_password_hash.as_ref()
        else {
            bail!("recovery password not yet set");
        };
        let Some(rack_network_config) = self.rack_network_config.as_ref() else {
            bail!("rack network config not set (have you uploaded a config?)");
        };
        let rack_network_config =
            validate_rack_network_config(rack_network_config);

        let known_bootstrap_sleds = bootstrap_peers.sleds();
        let mut bootstrap_ips = Vec::new();
        for sled in &self.bootstrap_sleds {
            let Some(ip) = known_bootstrap_sleds.get(&sled.baseboard).copied()
            else {
                bail!(
                    "IP address not (yet?) known for sled {} ({:?})",
                    sled.id.slot,
                    sled.baseboard,
                );
            };
            bootstrap_ips.push(ip);
        }

        // Convert between internal and progenitor types.
        let user_password_hash = bootstrap_agent_client::types::NewPasswordHash(
            recovery_silo_password_hash.to_string(),
        );
        let internal_services_ip_pool_ranges = self
            .internal_services_ip_pool_ranges
            .iter()
            .map(|pool| {
                use bootstrap_agent_client::types::IpRange;
                use bootstrap_agent_client::types::Ipv4Range;
                use bootstrap_agent_client::types::Ipv6Range;
                match pool {
                    address::IpRange::V4(range) => IpRange::V4(Ipv4Range {
                        first: range.first,
                        last: range.last,
                    }),
                    address::IpRange::V6(range) => IpRange::V6(Ipv6Range {
                        first: range.first,
                        last: range.last,
                    }),
                }
            })
            .collect();

        let request = RackInitializeRequest {
            rack_subnet: RACK_SUBNET,
            bootstrap_discovery: BootstrapAddressDiscovery::OnlyThese(
                bootstrap_ips,
            ),
            rack_secret_threshold: 1, // TODO REMOVE?
            ntp_servers: self.ntp_servers.clone(),
            dns_servers: self.dns_servers.clone(),
            internal_services_ip_pool_ranges,
            external_dns_zone_name: self.external_dns_zone_name.clone(),
            external_certificates: self.external_certificates.clone(),
            recovery_silo: RecoverySiloConfig {
                silo_name: Name::try_from(RECOVERY_SILO_NAME).unwrap(),
                user_name: UserId(RECOVERY_SILO_USERNAME.into()),
                user_password_hash,
            },
            rack_network_config: Some(rack_network_config),
        };

        Ok(request)
    }

    pub(crate) fn set_recovery_user_password_hash(
        &mut self,
        hash: omicron_passwords::NewPasswordHash,
    ) {
        self.recovery_silo_password_hash = Some(hash);
    }

    pub(crate) fn push_cert(
        &mut self,
        cert: Vec<u8>,
    ) -> Result<CertificateUploadResponse, String> {
        self.partial_external_certificate.cert = Some(cert);
        self.maybe_promote_external_certificate()
    }

    pub(crate) fn push_key(
        &mut self,
        key: Vec<u8>,
    ) -> Result<CertificateUploadResponse, String> {
        self.partial_external_certificate.key = Some(key);
        self.maybe_promote_external_certificate()
    }

    fn maybe_promote_external_certificate(
        &mut self,
    ) -> Result<CertificateUploadResponse, String> {
        // If we're still waiting on either the cert or the key, we have nothing
        // to do (but this isn't an error).
        let (cert, key) = match (
            self.partial_external_certificate.cert.as_ref(),
            self.partial_external_certificate.key.as_ref(),
        ) {
            (Some(cert), Some(key)) => (cert, key),
            (None, Some(_)) => {
                return Ok(CertificateUploadResponse::WaitingOnCert);
            }
            (Some(_), None) => {
                return Ok(CertificateUploadResponse::WaitingOnKey);
            }
            // We are only called by `push_key` or `push_cert`; one or the other
            // must be `Some(_)`.
            (None, None) => unreachable!(),
        };

        let mut validator = CertificateValidator::default();

        // We are running pre-NTP, so we can't check cert expirations; nexus
        // will have to do that.
        validator.danger_disable_expiration_validation();

        validator.validate(cert, key).map_err(|err| err.to_string())?;

        // Cert and key appear to be valid; steal them out of
        // `partial_external_certificate` and promote them to
        // `external_certificates`.
        self.external_certificates.push(Certificate {
            cert: self.partial_external_certificate.cert.take().unwrap(),
            key: self.partial_external_certificate.key.take().unwrap(),
        });

        Ok(CertificateUploadResponse::CertKeyAccepted)
    }

    pub(crate) fn update(
        &mut self,
        value: PutRssUserConfigInsensitive,
        our_baseboard: Option<&Baseboard>,
    ) -> Result<(), String> {
        // Updating can only fail in two ways:
        //
        // 1. If we have a real gimlet baseboard, that baseboard must be present
        //    in our inventory and in `value`'s list of sleds: we cannot exclude
        //    ourself from the rack.
        // 2. `value`'s bootstrap sleds includes sleds that aren't in our
        //    `inventory`.

        // First, confirm we have ourself in the inventory _and_ the user didn't
        // remove us from the list.
        if let Some(our_baseboard @ Baseboard::Gimlet { .. }) = our_baseboard {
            let our_slot = self
                .inventory
                .iter()
                .find_map(|sled| {
                    if sled.baseboard == *our_baseboard {
                        Some(sled.id.slot)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    format!(
                        "Inventory is missing the scrimlet where wicketd is \
                         running ({our_baseboard:?})",
                    )
                })?;
            if !value.bootstrap_sleds.contains(&our_slot) {
                return Err(format!(
                    "Cannot remove the scrimlet where wicketd is running \
                     (sled {our_slot}: {our_baseboard:?}) \
                     from bootstrap_sleds"
                ));
            }
        }

        // Next, confirm the user's list only consists of sleds in our
        // inventory.
        let mut bootstrap_sleds = BTreeSet::new();
        for slot in value.bootstrap_sleds {
            let sled =
                self.inventory
                    .iter()
                    .find(|sled| sled.id.slot == slot)
                    .ok_or_else(|| {
                        format!(
                            "cannot add unknown sled {slot} to bootstrap_sleds",
                        )
                    })?;
            bootstrap_sleds.insert(sled.clone());
        }

        self.bootstrap_sleds = bootstrap_sleds;
        self.ntp_servers = value.ntp_servers;
        self.dns_servers = value.dns_servers;
        self.internal_services_ip_pool_ranges =
            value.internal_services_ip_pool_ranges;
        self.external_dns_zone_name = value.external_dns_zone_name;
        self.rack_network_config = Some(value.rack_network_config);

        Ok(())
    }
}

impl From<&'_ CurrentRssConfig> for CurrentRssUserConfig {
    fn from(rss: &CurrentRssConfig) -> Self {
        // If the user has selected bootstrap sleds, use those; otherwise,
        // default to the full inventory list.
        let bootstrap_sleds = if !rss.bootstrap_sleds.is_empty() {
            rss.bootstrap_sleds.clone()
        } else {
            rss.inventory.clone()
        };

        Self {
            sensitive: CurrentRssUserConfigSensitive {
                num_external_certificates: rss.external_certificates.len(),
                recovery_silo_password_set: rss
                    .recovery_silo_password_hash
                    .is_some(),
            },
            insensitive: CurrentRssUserConfigInsensitive {
                bootstrap_sleds,
                ntp_servers: rss.ntp_servers.clone(),
                dns_servers: rss.dns_servers.clone(),
                internal_services_ip_pool_ranges: rss
                    .internal_services_ip_pool_ranges
                    .clone(),
                external_dns_zone_name: rss.external_dns_zone_name.clone(),
                rack_network_config: rss.rack_network_config.clone(),
            },
        }
    }
}

fn validate_rack_network_config(
    config: &RackNetworkConfig,
) -> bootstrap_agent_client::types::RackNetworkConfig {
    use bootstrap_agent_client::types::PortFec as BaPortFec;
    use bootstrap_agent_client::types::PortSpeed as BaPortSpeed;
    use omicron_common::api::internal::shared::PortFec;
    use omicron_common::api::internal::shared::PortSpeed;

    // TODO Add client side checks on `rack_network_config` contents.

    bootstrap_agent_client::types::RackNetworkConfig {
        gateway_ip: config.gateway_ip.clone(),
        infra_ip_first: config.infra_ip_first.clone(),
        infra_ip_last: config.infra_ip_last.clone(),
        uplink_port: config.uplink_port.clone(),
        uplink_port_speed: match config.uplink_port_speed {
            PortSpeed::Speed0G => BaPortSpeed::Speed0G,
            PortSpeed::Speed1G => BaPortSpeed::Speed1G,
            PortSpeed::Speed10G => BaPortSpeed::Speed10G,
            PortSpeed::Speed25G => BaPortSpeed::Speed25G,
            PortSpeed::Speed40G => BaPortSpeed::Speed40G,
            PortSpeed::Speed50G => BaPortSpeed::Speed50G,
            PortSpeed::Speed100G => BaPortSpeed::Speed100G,
            PortSpeed::Speed200G => BaPortSpeed::Speed200G,
            PortSpeed::Speed400G => BaPortSpeed::Speed400G,
        },
        uplink_port_fec: match config.uplink_port_fec {
            PortFec::Firecode => BaPortFec::Firecode,
            PortFec::None => BaPortFec::None,
            PortFec::Rs => BaPortFec::Rs,
        },
        uplink_ip: config.uplink_ip.clone(),
        uplink_vid: config.uplink_vid,
    }
}