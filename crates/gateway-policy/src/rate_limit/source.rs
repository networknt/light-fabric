//! Transport-neutral, explicitly configured proxy trust for quota identities.
use http::HeaderMap;
use ipnet::IpNet;
use std::net::IpAddr;

#[derive(Clone, Default)]
pub struct TrustedProxyPolicy {
    networks: Vec<IpNet>,
}
impl TrustedProxyPolicy {
    pub fn new(cidrs: &[String]) -> Result<Self, &'static str> {
        if cidrs.len() > 64 {
            return Err("limit.yml: too many trusted proxy CIDRs");
        }
        let networks = cidrs
            .iter()
            .map(|value| {
                let network: IpNet = value
                    .parse()
                    .map_err(|_| "limit.yml: invalid trusted proxy CIDR")?;
                let network = match network {
                    IpNet::V6(value) if value.prefix_len() >= 96 => {
                        match value.addr().to_ipv4_mapped() {
                            Some(address) => {
                                IpNet::new(IpAddr::V4(address), value.prefix_len() - 96)
                                    .map_err(|_| "limit.yml: invalid trusted proxy CIDR")?
                            }
                            None => network,
                        }
                    }
                    _ => network,
                };
                if network.prefix_len() == 0 {
                    return Err("limit.yml: unrestricted proxy trust is forbidden");
                }
                Ok(network)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { networks })
    }
    pub fn is_empty(&self) -> bool {
        self.networks.is_empty()
    }
    fn trusts(&self, address: IpAddr) -> bool {
        self.networks
            .iter()
            .any(|network| network.contains(&address))
    }
    /// The peer must come from the transport/admitted ingress, never a header.
    /// Only allowlisted peers can assert an end-client address. Akamai mode
    /// requires a sanitized, single-valued True-Client-IP from that proxy.
    pub fn resolve(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        akamai: bool,
    ) -> Result<IpAddr, &'static str> {
        if akamai && self.is_empty() {
            return Err("Akamai rate limiting requires trustedProxyCidrs");
        }
        let peer = canonical(peer.ok_or("client source address is unavailable")?);
        if !self.trusts(peer) {
            return if akamai {
                Err("Akamai client address requires a trusted proxy peer")
            } else {
                Ok(peer)
            };
        }
        if akamai {
            let mut values = headers.get_all("true-client-ip").iter();
            let value = values
                .next()
                .ok_or("trusted proxy client address is missing")?;
            if values.next().is_some() {
                return Err("trusted proxy client address is ambiguous");
            }
            let value = value
                .to_str()
                .map_err(|_| "trusted proxy client address is invalid")?;
            return parse(value);
        }
        let mut current = peer;
        let mut hops = 0;
        let mut bytes = 0usize;
        for value in headers.get_all("x-forwarded-for").iter().rev() {
            bytes = bytes.saturating_add(value.as_bytes().len());
            if bytes > 8192 {
                return Err("trusted proxy chain is too large");
            }
            let value = value
                .to_str()
                .map_err(|_| "trusted proxy chain is invalid")?;
            for hop in value.rsplit(',') {
                if !self.trusts(current) {
                    return Ok(current);
                }
                hops += 1;
                if hops > 64 {
                    return Err("trusted proxy chain has too many hops");
                }
                current = parse(hop)?;
                if !self.trusts(current) {
                    return Ok(current);
                }
            }
        }
        if hops == 0 || self.trusts(current) {
            return Err("trusted proxy chain has no client address");
        }
        Ok(current)
    }
}
fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(value) => value.to_ipv4_mapped().map_or(address, IpAddr::V4),
        _ => address,
    }
}
fn parse(value: &str) -> Result<IpAddr, &'static str> {
    let address: IpAddr = value
        .trim()
        .parse()
        .map_err(|_| "trusted proxy client address is invalid")?;
    let address = canonical(address);
    if address.is_unspecified() || address.is_multicast() {
        return Err("trusted proxy client address is invalid");
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proxy() -> TrustedProxyPolicy {
        TrustedProxyPolicy::new(&["203.0.113.0/24".into()]).unwrap()
    }
    fn headers(name: &str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        headers
    }
    #[test]
    fn untrusted_peer_cannot_choose_either_forwarded_identity() {
        for akamai in [false, true] {
            let mut headers = headers("x-forwarded-for", "1.2.3.4, 5.6.7.8");
            headers.insert("true-client-ip", "9.8.7.6".parse().unwrap());
            let result = proxy().resolve(&headers, Some("198.51.100.7".parse().unwrap()), akamai);
            if akamai {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().to_string(), "198.51.100.7");
            }
        }
    }
    #[test]
    fn trusted_chain_stops_at_first_untrusted_hop_from_the_right() {
        let headers = headers("x-forwarded-for", "spoofed, 198.51.100.7, 203.0.113.8");
        assert_eq!(
            proxy()
                .resolve(&headers, Some("203.0.113.9".parse().unwrap()), false)
                .unwrap()
                .to_string(),
            "198.51.100.7"
        );
    }
    #[test]
    fn trusted_cdn_clients_are_separate_and_malformed_assertions_fail_closed() {
        for client in ["198.51.100.7", "198.51.100.8", "2001:db8::1"] {
            assert_eq!(
                proxy()
                    .resolve(
                        &headers("true-client-ip", client),
                        Some("203.0.113.9".parse().unwrap()),
                        true
                    )
                    .unwrap()
                    .to_string(),
                client
            );
        }
        for value in ["invalid", "198.51.100.7, 198.51.100.8", "0.0.0.0"] {
            assert!(
                proxy()
                    .resolve(
                        &headers("true-client-ip", value),
                        Some("203.0.113.9".parse().unwrap()),
                        true
                    )
                    .is_err()
            );
        }
        let mut duplicate = headers("true-client-ip", "198.51.100.7");
        duplicate.append("true-client-ip", "198.51.100.8".parse().unwrap());
        assert!(
            proxy()
                .resolve(&duplicate, Some("203.0.113.9".parse().unwrap()), true)
                .is_err()
        );
        assert!(
            proxy()
                .resolve(
                    &HeaderMap::new(),
                    Some("203.0.113.9".parse().unwrap()),
                    true
                )
                .is_err()
        );
        assert!(
            TrustedProxyPolicy::default()
                .resolve(
                    &HeaderMap::new(),
                    Some("203.0.113.9".parse().unwrap()),
                    true
                )
                .is_err()
        );
    }
    #[test]
    fn ipv6_proxy_and_canonical_client_forms_use_the_same_identity() {
        let trust = TrustedProxyPolicy::new(&["2001:db8:ffff::/48".into()]).unwrap();
        let peer = Some("2001:db8:ffff::9".parse().unwrap());
        assert_eq!(
            trust
                .resolve(
                    &headers("true-client-ip", "::ffff:198.51.100.7"),
                    peer,
                    true
                )
                .unwrap()
                .to_string(),
            "198.51.100.7"
        );
        let mut chain = headers("x-forwarded-for", "spoofed");
        chain.append(
            "x-forwarded-for",
            "2001:db8:1::7, 2001:db8:ffff::8".parse().unwrap(),
        );
        assert_eq!(
            trust.resolve(&chain, peer, false).unwrap().to_string(),
            "2001:db8:1::7"
        );
        assert!(trust.resolve(&HeaderMap::new(), None, false).is_err());
    }
    #[test]
    fn mapped_proxy_cidrs_are_canonicalized_before_trust_checks() {
        let trust = TrustedProxyPolicy::new(&["::ffff:203.0.113.0/120".into()]).unwrap();
        assert_eq!(
            trust
                .resolve(
                    &headers("true-client-ip", "198.51.100.7"),
                    Some("::ffff:203.0.113.9".parse().unwrap()),
                    true
                )
                .unwrap()
                .to_string(),
            "198.51.100.7"
        );
        assert!(TrustedProxyPolicy::new(&["::ffff:0.0.0.0/96".into()]).is_err());
    }
    #[test]
    fn trust_configuration_and_chain_work_are_bounded() {
        for value in ["invalid", "0.0.0.0/0", "::/0"] {
            assert!(TrustedProxyPolicy::new(&[value.into()]).is_err());
        }
        assert!(TrustedProxyPolicy::new(&vec!["203.0.113.0/24".into(); 65]).is_err());
        let chain = std::iter::repeat_n("203.0.113.9", 65)
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            proxy()
                .resolve(
                    &headers("x-forwarded-for", &chain),
                    Some("203.0.113.9".parse().unwrap()),
                    false
                )
                .is_err()
        );
    }
}
