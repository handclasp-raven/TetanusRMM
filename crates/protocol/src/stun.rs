//! The part of STUN (RFC 5389) that candidate gathering needs: a Binding
//! request, and a success response carrying the address the request came
//! from (`XOR-MAPPED-ADDRESS`), which is the requester's public address
//! when it sits behind NAT.
//!
//! The server answers these on its STUN port; agents and viewers ask it
//! from the UDP socket they will use for a direct path. Responses from any
//! standard STUN server parse too (plain `MAPPED-ADDRESS` is accepted as a
//! fallback), so a public STUN server works as well.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;

/// Identifies a request and its response.
pub type TransactionId = [u8; 12];

/// A Binding request with no attributes.
pub fn binding_request(txid: &TransactionId) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Length 0: no attributes.
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

/// A well-formed STUN message header: type, attribute length, transaction.
fn header(packet: &[u8]) -> Option<(u16, &[u8], TransactionId)> {
    if packet.len() < HEADER_LEN || packet[0] & 0xC0 != 0 {
        return None;
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    let len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let cookie = u32::from_be_bytes(packet[4..8].try_into().ok()?);
    if cookie != MAGIC_COOKIE || len % 4 != 0 || packet.len() != HEADER_LEN + len {
        return None;
    }
    Some((kind, &packet[HEADER_LEN..], packet[8..20].try_into().ok()?))
}

/// The transaction id, if `packet` is a Binding request. Attributes the
/// client included (SOFTWARE, FINGERPRINT, ...) are ignored.
pub fn parse_binding_request(packet: &[u8]) -> Option<TransactionId> {
    match header(packet)? {
        (BINDING_REQUEST, _, txid) => Some(txid),
        _ => None,
    }
}

/// A success response telling the requester it was seen as `mapped`.
pub fn binding_response(txid: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
    let port = mapped.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut value = vec![0u8];
    match mapped.ip() {
        IpAddr::V4(ip) => {
            value.push(FAMILY_V4);
            value.extend_from_slice(&port.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC_COOKIE;
            value.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            value.push(FAMILY_V6);
            value.extend_from_slice(&port.to_be_bytes());
            let mask = v6_mask(txid);
            value.extend(ip.octets().iter().zip(mask).map(|(a, m)| a ^ m));
        }
    }
    let mut out = Vec::with_capacity(HEADER_LEN + 4 + value.len());
    out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(&value);
    out
}

fn v6_mask(txid: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(txid);
    mask
}

/// The mapped address from a success response to transaction `txid`.
pub fn parse_binding_response(packet: &[u8], txid: &TransactionId) -> Option<SocketAddr> {
    let (kind, mut attrs, got) = header(packet)?;
    if kind != BINDING_SUCCESS || &got != txid {
        return None;
    }
    let mut plain = None;
    while attrs.len() >= 4 {
        let kind = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = usize::from(u16::from_be_bytes([attrs[2], attrs[3]]));
        let padded = len.div_ceil(4) * 4;
        let value = attrs.get(4..4 + len)?;
        match kind {
            ATTR_XOR_MAPPED_ADDRESS => return address(value, Some(txid)),
            ATTR_MAPPED_ADDRESS => plain = address(value, None),
            _ => {}
        }
        attrs = attrs.get(4 + padded..).unwrap_or_default();
    }
    plain
}

/// Decode a (XOR-)MAPPED-ADDRESS value; `xor` carries the transaction id
/// for the XOR form.
fn address(value: &[u8], xor: Option<&TransactionId>) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let mut port = u16::from_be_bytes(value.get(2..4)?.try_into().ok()?);
    if xor.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match family {
        FAMILY_V4 => {
            let mut raw = u32::from_be_bytes(value.get(4..8)?.try_into().ok()?);
            if xor.is_some() {
                raw ^= MAGIC_COOKIE;
            }
            IpAddr::V4(Ipv4Addr::from(raw))
        }
        FAMILY_V6 => {
            let mut raw: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            if let Some(txid) = xor {
                for (b, m) in raw.iter_mut().zip(v6_mask(txid)) {
                    *b ^= m;
                }
            }
            IpAddr::V6(Ipv6Addr::from(raw))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: TransactionId = *b"0123456789ab";

    #[test]
    fn requests_and_responses_round_trip_for_both_families() {
        let request = binding_request(&TXID);
        assert_eq!(parse_binding_request(&request), Some(TXID));
        for mapped in [
            "203.0.113.7:61000".parse().unwrap(),
            "[2001:db8::1:2]:443".parse().unwrap(),
        ] {
            let response = binding_response(&TXID, mapped);
            assert_eq!(parse_binding_response(&response, &TXID), Some(mapped));
            // Not a request, and not an answer to some other transaction.
            assert_eq!(parse_binding_request(&response), None);
            assert_eq!(parse_binding_response(&response, b"other-txid!!"), None);
        }
    }

    #[test]
    fn matches_the_rfc_5769_ipv4_response_vector() {
        // RFC 5769 section 2.2, minus SOFTWARE/MESSAGE-INTEGRITY/FINGERPRINT
        // (which we would skip anyway): XOR-MAPPED-ADDRESS 192.0.2.1:32853.
        let txid: TransactionId = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut packet = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        packet.extend_from_slice(&txid);
        packet.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        assert_eq!(
            parse_binding_response(&packet, &txid),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert_eq!(
            binding_response(&txid, "192.0.2.1:32853".parse().unwrap()),
            packet
        );
    }

    #[test]
    fn requests_with_attributes_are_still_answered() {
        let mut request = binding_request(&TXID).to_vec();
        // SOFTWARE "rmm" padded to 4 bytes.
        request.extend_from_slice(&[0x80, 0x22, 0x00, 0x03, b'r', b'm', b'm', 0]);
        request[3] = 8;
        assert_eq!(parse_binding_request(&request), Some(TXID));
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(parse_binding_request(b""), None);
        assert_eq!(parse_binding_request(&[0u8; 20]), None, "no magic cookie");
        let mut request = binding_request(&TXID);
        request[3] = 4; // claims an attribute that is not there
        assert_eq!(parse_binding_request(&request), None);
        // A QUIC long-header packet starts with 0b11: never STUN.
        let mut quic = binding_request(&TXID);
        quic[0] = 0xC3;
        assert_eq!(parse_binding_request(&quic), None);
        // A truncated attribute does not panic.
        let mut response = binding_response(&TXID, "192.0.2.1:1".parse().unwrap());
        response.truncate(response.len() - 2);
        let len = (response.len() - HEADER_LEN) as u16;
        response[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_binding_response(&response, &TXID), None);
    }
}
