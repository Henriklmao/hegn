//! TAP-device naming and the `/30` address slice for each pool entry.
//!
//! Every VM slot owns a data TAP (`hegn<i>`, guarded networking) and a control
//! TAP (`hegnc<i>`, always present). The two pools use separate `/24`
//! subnets and separate MAC ranges so a guest can tell the interfaces apart
//! without relying on the host's predictable interface naming.

use std::net::Ipv4Addr;

use super::{SUBNET, TAP_POOL};

/// Root of the control network. Every control TAP gets a `/30` slice, just like
/// the guarded data network, but on a separate subnet.
pub const CONTROL_SUBNET: Ipv4Addr = Ipv4Addr::new(10, 214, 0, 0);
/// Number of control TAP slots. It mirrors [`TAP_POOL`] so a slot index is
/// valid for both networks.
pub const CONTROL_TAP_POOL: u8 = TAP_POOL;

fn check_index(index: u8, pool: u8, kind: &str) {
    assert!(
        index < pool,
        "{kind} TAP index {index} is outside the {pool}-slot pool"
    );
}

pub fn tap_name(index: u8) -> String {
    check_index(index, TAP_POOL, "data");
    format!("hegn{index}")
}

pub fn host_ip(index: u8) -> Ipv4Addr {
    check_index(index, TAP_POOL, "data");
    slice_ip(SUBNET, index, 1)
}

pub fn guest_ip(index: u8) -> Ipv4Addr {
    check_index(index, TAP_POOL, "data");
    slice_ip(SUBNET, index, 2)
}

pub fn mac(index: u8) -> String {
    check_index(index, TAP_POOL, "data");
    format!("52:54:00:00:00:{:02x}", index + 1)
}

pub fn control_tap_name(index: u8) -> String {
    check_index(index, CONTROL_TAP_POOL, "control");
    format!("hegnc{index}")
}

pub fn control_host_ip(index: u8) -> Ipv4Addr {
    check_index(index, CONTROL_TAP_POOL, "control");
    slice_ip(CONTROL_SUBNET, index, 1)
}

pub fn control_guest_ip(index: u8) -> Ipv4Addr {
    check_index(index, CONTROL_TAP_POOL, "control");
    slice_ip(CONTROL_SUBNET, index, 2)
}

pub fn control_mac(index: u8) -> String {
    check_index(index, CONTROL_TAP_POOL, "control");
    format!("52:54:00:00:01:{:02x}", index + 1)
}

fn slice_ip(subnet: Ipv4Addr, index: u8, offset: u8) -> Ipv4Addr {
    let octets = subnet.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], 4 * index + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_math_is_a_30_slice_per_pool_entry() {
        assert_eq!(host_ip(0), Ipv4Addr::new(10, 213, 0, 1));
        assert_eq!(guest_ip(0), Ipv4Addr::new(10, 213, 0, 2));
        assert_eq!(host_ip(1), Ipv4Addr::new(10, 213, 0, 5));
        assert_eq!(guest_ip(15), Ipv4Addr::new(10, 213, 0, 62));
        assert_eq!(mac(0), "52:54:00:00:00:01");
        assert_eq!(mac(15), "52:54:00:00:00:10");
        assert_eq!(tap_name(0), "hegn0");
        assert_eq!(tap_name(15), "hegn15");
    }

    #[test]
    fn control_address_math_uses_its_own_subnet_and_mac_range() {
        assert_eq!(CONTROL_SUBNET, Ipv4Addr::new(10, 214, 0, 0));
        assert_eq!(CONTROL_TAP_POOL, TAP_POOL);
        assert_eq!(control_tap_name(0), "hegnc0");
        assert_eq!(control_tap_name(15), "hegnc15");
        assert_eq!(control_host_ip(0), Ipv4Addr::new(10, 214, 0, 1));
        assert_eq!(control_guest_ip(0), Ipv4Addr::new(10, 214, 0, 2));
        assert_eq!(control_host_ip(1), Ipv4Addr::new(10, 214, 0, 5));
        assert_eq!(control_guest_ip(15), Ipv4Addr::new(10, 214, 0, 62));
        assert_eq!(control_mac(0), "52:54:00:00:01:01");
        assert_eq!(control_mac(15), "52:54:00:00:01:10");
    }

    #[test]
    #[should_panic(expected = "outside the 16-slot pool")]
    fn data_addressing_rejects_an_out_of_bounds_index() {
        let _ = tap_name(16);
    }

    #[test]
    #[should_panic(expected = "outside the 16-slot pool")]
    fn control_addressing_rejects_an_out_of_bounds_index() {
        let _ = control_tap_name(16);
    }
}
