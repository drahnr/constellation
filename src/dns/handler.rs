// Constellation
//
// Pluggable authoritative DNS server
// Copyright: 2018, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use log;
use std::collections::HashMap;
use std::sync::RwLock;
use trust_dns::op::{Message, MessageType, OpCode, Query, ResponseCode};
use trust_dns::rr::{Name, Record, RecordType as TrustRecordType};
use trust_dns::rr::dnssec::SupportedAlgorithms;
use trust_dns_server::server::{Request, RequestHandler};
use trust_dns_server::authority::{AuthLookup, Authority};

use dns::zone::ZoneName;
use dns::record::{RecordName, RecordType};
use store::store::StoreRecord;
use APP_CONF;
use APP_STORE;

pub struct DNSHandler {
    authorities: HashMap<Name, RwLock<Authority>>,
}

impl RequestHandler for DNSHandler {
    fn handle_request(&self, request: &Request) -> Message {
        let request_message = &request.message;

        log::trace!("request: {:?}", request_message);

        let response: Message = match request_message.message_type() {
            MessageType::Query => {
                match request_message.op_code() {
                    OpCode::Query => {
                        let response = self.lookup(&request_message);

                        log::trace!("query response: {:?}", response);

                        response
                    }
                    code @ _ => {
                        log::error!("unimplemented opcode: {:?}", code);

                        Message::error_msg(
                            request_message.id(),
                            request_message.op_code(),
                            ResponseCode::NotImp,
                        )
                    }
                }
            }
            MessageType::Response => {
                log::warn!(
                    "got a response as a request from id: {}",
                    request_message.id()
                );

                Message::error_msg(
                    request_message.id(),
                    request_message.op_code(),
                    ResponseCode::NotImp,
                )
            }
        };

        response
    }
}

impl DNSHandler {
    pub fn new() -> Self {
        DNSHandler { authorities: HashMap::new() }
    }

    pub fn upsert(&mut self, name: Name, authority: Authority) {
        self.authorities.insert(name, RwLock::new(authority));
    }

    pub fn lookup(&self, request: &Message) -> Message {
        let mut response: Message = Message::new();

        response.set_id(request.id());
        response.set_op_code(OpCode::Query);
        response.set_message_type(MessageType::Response);
        response.add_queries(request.queries().into_iter().cloned());

        for query in request.queries() {
            if let Some(ref_authority) = self.find_auth_recurse(query.name()) {
                let authority = &ref_authority.read().unwrap();

                log::info!(
                    "request: {} found authority: {}",
                    request.id(),
                    authority.origin()
                );

                let supported_algorithms = SupportedAlgorithms::new();

                // Attempt to resolve from local store
                let records_local = authority.search(query, false, supported_algorithms);

                if !records_local.is_empty() {
                    log::debug!("found records for query from local store: {}", query);

                    let records_local_vec = records_local
                        .iter()
                        .map(|record| record.to_owned())
                        .collect();

                    Self::serve_response_records(
                        &mut response,
                        records_local_vec,
                        &authority,
                        supported_algorithms,
                    );
                } else {
                    if let Some(records_remote) = Self::records_from_store(authority, query) {
                        log::debug!("found records for query from remote store: {}", query);

                        Self::serve_response_records(
                            &mut response,
                            records_remote,
                            &authority,
                            supported_algorithms,
                        );
                    } else {
                        log::debug!("did not find records for query: {}", query);

                        match records_local {
                            AuthLookup::NoName => {
                                log::debug!("domain not found for query: {}", query);

                                response.set_response_code(ResponseCode::NXDomain)
                            }
                            AuthLookup::NameExists => {
                                log::debug!("domain found for query: {}", query);

                                response.set_response_code(ResponseCode::NoError)
                            }
                            AuthLookup::Records(..) => panic!("error, should return noerror"),
                        };

                        let soa_records = authority.soa_secure(false, supported_algorithms);

                        if soa_records.is_empty() {
                            log::warn!("no soa record for: {:?}", authority.origin());
                        } else {
                            response.add_name_servers(soa_records.iter().cloned());
                        }
                    }
                }
            } else {
                log::debug!("domain authority not found for query: {}", query);

                response.set_response_code(ResponseCode::NXDomain);
            }
        }

        response
    }

    fn find_auth_recurse(&self, name: &Name) -> Option<&RwLock<Authority>> {
        let authority = self.authorities.get(name);

        if authority.is_some() {
            return authority;
        } else {
            let name = name.base_name();

            if !name.is_root() {
                return self.find_auth_recurse(&name);
            }
        }

        None
    }

    fn records_from_store(authority: &Authority, query: &Query) -> Option<Vec<Record>> {
        let (query_name, query_type) = (query.name(), query.query_type());

        // Attempt with requested domain
        let mut records =
            Self::records_from_store_attempt(authority, &query_name, &query_name, &query_type);

        // Attempt with wildcard domain
        if records.is_none() {
            if let Some(base_name) = query_name.to_string().splitn(2, ".").nth(1) {
                let wildcard_name_string = format!("*.{}", base_name);

                if let Ok(wildcard_name) = Name::parse(&wildcard_name_string, Some(&Name::new())) {
                    if &wildcard_name != query_name {
                        records = Self::records_from_store_attempt(
                            authority,
                            &query_name,
                            &wildcard_name,
                            &query_type,
                        )
                    }
                }
            }
        }

        records
    }

    fn records_from_store_attempt(
        authority: &Authority,
        query_name_client: &Name,
        query_name_effective: &Name,
        query_type: &TrustRecordType,
    ) -> Option<Vec<Record>> {
        let zone_name = ZoneName::from_trust(&authority.origin());
        let record_name = RecordName::from_trust(&authority.origin(), query_name_effective);
        let record_type = RecordType::from_trust(query_type);

        log::debug!(
            "lookup record in store for query: {} {} on zone: {:?}, record: {:?}, and type: {:?}",
            query_name_effective,
            query_type,
            zone_name,
            record_name,
            record_type
        );

        match (zone_name, record_name, record_type) {
            (Some(zone_name), Some(record_name), Some(record_type)) => {
                let mut records = Vec::new();

                if let Ok(record) = APP_STORE.get(&zone_name, &record_name, &record_type) {
                    log::debug!(
                        "found record in store for query: {} {} with result: {:?}",
                        query_name_effective,
                        query_type,
                        record
                    );

                    // Append record direct results
                    Self::parse_from_records(query_name_client, &record, &mut records);
                }

                // Look for a CNAME result?
                if record_type != RecordType::CNAME {
                    if let Ok(record_cname) = APP_STORE.get(
                        &zone_name,
                        &record_name,
                        &RecordType::CNAME,
                    )
                    {
                        log::debug!(
                            "found cname hint record in store for query: {} {} with result: {:?}",
                            query_name_effective,
                            query_type,
                            record_cname
                        );

                        // Append CNAME hint results
                        Self::parse_from_records(query_name_client, &record_cname, &mut records);
                    }
                }

                if !records.is_empty() {
                    return Some(records);
                }
            }
            _ => {}
        };

        None
    }

    fn parse_from_records(
        query_name_client: &Name,
        record: &StoreRecord,
        records: &mut Vec<Record>,
    ) {
        if let Ok(type_data) = record.kind.to_trust() {
            for value in record.values.iter() {
                if let Ok(value_data) = value.to_trust(&record.kind) {
                    records.push(Record::from_rdata(
                        query_name_client.to_owned(),
                        record.ttl.unwrap_or(APP_CONF.dns.record_ttl),
                        type_data,
                        value_data,
                    ));
                } else {
                    log::warn!(
                        "could not convert to dns record type: {} with value: {:?}",
                        record.kind.to_str(),
                        value
                    );
                }
            }
        } else {
            log::warn!(
                "could not convert to dns record type: {}",
                record.kind.to_str()
            );
        }
    }

    fn serve_response_records(
        response: &mut Message,
        records: Vec<Record>,
        authority: &Authority,
        supported_algorithms: SupportedAlgorithms,
    ) {
        response.set_response_code(ResponseCode::NoError);
        response.set_authoritative(true);
        response.add_answers(records);

        let ns_records = authority.ns(false, supported_algorithms);

        if ns_records.is_empty() {
            log::warn!("no ns records for: {:?}", authority.origin());
        } else {
            response.add_name_servers(ns_records.iter().cloned());
        }
    }
}
