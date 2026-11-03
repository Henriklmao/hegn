//! TAP-device naming and the `/30` address slice for each pool entry.

use std::net::Ipv4Addr;

use super::SUBNET;

pub fn tap_name(index: u8) -> String {
    format!("hegn{index}")
}

pub fn host_ip(index: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        SUBNET.octets()[0],
        SUBNET.octets()[1],
        SUBNET.octets()[2],
        4 * index + 1,
    )
}

pub fn guest_ip(index: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        SUBNET.octets()[0],
        SUBNET.octets()[1],
        SUBNET.octets()[2],
        4 * index + 2,
    )
}

pub fn mac(index: u8) -> String {
    format!("52:54:00:00:00:{:02x}", index + 1)
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
    }
}
