// Constellation
//
// Pluggable authoritative DNS server
// Copyright: 2018, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use std::collections::BTreeMap;
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;
use trust_dns_proto::rr::rdata::SOA;
use trust_dns_proto::rr::{Name, RData, Record, RecordSet, RecordType};
use trust_dns_server::authority::{Authority, ZoneType};
use trust_dns_server::server::ServerFuture;

use super::handler::Authority2;

use super::handler::DNSHandler;
use crate::APP_CONF;

lazy_static! {
    static ref NAME_SOA_MASTER: Name =
        Name::parse(&APP_CONF.dns.soa_master, Some(&Name::new())).expect("invalid soa master");
    static ref NAME_SOA_RESPONSIBLE: Name =
        Name::parse(&APP_CONF.dns.soa_responsible, Some(&Name::new()))
            .expect("invalid soa responsible");
}

static SERIAL_DEFAULT: u32 = 1;

pub struct DNSListenBuilder;
pub struct DNSListen;

impl DNSListenBuilder {
    pub fn new() -> DNSListen {
        DNSListen {}
    }
}

impl DNSListen {
    pub fn run(&self) {
        // Run the DNS server
        let mut handler: DNSHandler = DNSHandler::new();

        for (zone_name, _) in &APP_CONF.dns.zone {
            match Self::map_authority(&zone_name) {
                Ok((name, authority)) => handler.upsert(name, authority),
                Err(_) => error!("could not load zone {}", zone_name),
            }
        }

        let mut server = ServerFuture::new(handler).expect("error creating dns server");

        // Register sockets & listeners
        for inet in &APP_CONF.dns.inets {
            let udp_socket = UdpSocket::bind(inet).expect(&format!("udp bind failed: {}", inet));
            let tcp_listener =
                TcpListener::bind(inet).expect(&format!("tcp bind failed: {}", inet));

            info!("listening for udp on {:?}", udp_socket);
            server.register_socket(udp_socket);

            info!("listening for tcp on {:?}", tcp_listener);
            server
                .register_listener(tcp_listener, Duration::from_secs(APP_CONF.dns.tcp_timeout))
                .expect("could not register tcp listener");
        }

        // Listen for connections
        info!("listening for dns connections");

        if let Err(err) = server.listen() {
            error!("failed to listen on dns: {}", err);
        }
    }

    fn map_authority(zone_name: &str) -> Result<(Name, Authority2), ()> {
        if let Ok(name) = Name::parse(zone_name, Some(&Name::new())) {
            let mut records = BTreeMap::new();

            // Insert base SOA records
            let soa_records = RecordSet::from(Record::from_rdata(
                name.to_owned(),
                APP_CONF.dns.record_ttl,
                RecordType::SOA,
                RData::SOA(SOA::new(
                    NAME_SOA_MASTER.to_owned(),
                    NAME_SOA_RESPONSIBLE.to_owned(),
                    SERIAL_DEFAULT,
                    APP_CONF.dns.soa_refresh,
                    APP_CONF.dns.soa_retry,
                    APP_CONF.dns.soa_expire,
                    APP_CONF.dns.soa_ttl,
                )),
            ));

            records.insert(RecordSet::new(&name, RecordType::SOA, 1337), soa_records);

            // Insert base NS records
            let mut ns_records = RecordSet::new(&name, RecordType::NS, SERIAL_DEFAULT);

            for nameserver in &APP_CONF.dns.nameservers {
                ns_records.insert(
                    Record::from_rdata(
                        name.to_owned(),
                        APP_CONF.dns.record_ttl,
                        RecordType::NS,
                        RData::NS(
                            Name::parse(nameserver, Some(&Name::new()))
                                .expect("invalid nameserver"),
                        ),
                    ),
                    SERIAL_DEFAULT,
                );
            }

            records.insert(RecordSet::new(&name, RecordType::NS, 1337), ns_records);

            Ok((
                name.to_owned(),
                Authority2::new(name, records, ZoneType::Master, false, false),
            ))
        } else {
            Err(())
        }
    }
}
