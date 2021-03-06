// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::vec::IntoIter;

use crc::crc64::{self, Digest, Hasher64};
use kvproto::coprocessor::{KeyRange, Response};
use protobuf::Message;
use tipb::checksum::{ChecksumAlgorithm, ChecksumRequest, ChecksumResponse, ChecksumScanOn};

use storage::{Snapshot, SnapshotStore};

use super::dag::executor::{ExecutorMetrics, ScanOn, Scanner};
use super::endpoint::ReqContext;
use super::{Error, Result};

// `ChecksumContext` is used to handle `ChecksumRequest`
pub struct ChecksumContext {
    req: ChecksumRequest,
    store: SnapshotStore,
    ranges: IntoIter<KeyRange>,
    scanner: Option<Scanner>,
}

impl ChecksumContext {
    pub fn new(
        req: ChecksumRequest,
        ranges: Vec<KeyRange>,
        snap: Box<Snapshot>,
        ctx: &ReqContext,
    ) -> ChecksumContext {
        let store = SnapshotStore::new(
            snap,
            req.get_start_ts(),
            ctx.isolation_level,
            ctx.fill_cache,
        );
        ChecksumContext {
            req,
            store,
            ranges: ranges.into_iter(),
            scanner: None,
        }
    }

    pub fn handle_request(mut self, metrics: &mut ExecutorMetrics) -> Result<Response> {
        let algorithm = self.req.get_algorithm();
        if algorithm != ChecksumAlgorithm::Crc64_Xor {
            return Err(box_err!("unknown checksum algorithm {:?}", algorithm));
        }

        let mut checksum = 0;
        let mut total_kvs = 0;
        let mut total_bytes = 0;
        while let Some((k, v)) = self.next_row(metrics)? {
            checksum = checksum_crc64_xor(checksum, &k, &v);
            total_kvs += 1;
            total_bytes += k.len() + v.len();
        }

        let mut resp = ChecksumResponse::new();
        resp.set_checksum(checksum);
        resp.set_total_kvs(total_kvs);
        resp.set_total_bytes(total_bytes as u64);
        let data = box_try!(resp.write_to_bytes());

        let mut resp = Response::new();
        resp.set_data(data);
        Ok(resp)
    }

    fn next_row(&mut self, metrics: &mut ExecutorMetrics) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(scanner) = self.scanner.as_mut() {
                metrics.scan_counter.inc_range();
                match scanner.next_row()? {
                    Some(row) => return Ok(Some(row)),
                    None => scanner.collect_statistics_into(&mut metrics.cf_stats),
                }
            }

            if let Some(range) = self.ranges.next() {
                self.scanner = match self.scanner.take() {
                    Some(mut scanner) => {
                        box_try!(scanner.reset_range(range, &self.store));
                        Some(scanner)
                    }
                    None => Some(self.new_scanner(range)?),
                };
                continue;
            }

            return Ok(None);
        }
    }

    fn new_scanner(&self, range: KeyRange) -> Result<Scanner> {
        let scan_on = match self.req.get_scan_on() {
            ChecksumScanOn::Table => ScanOn::Table,
            ChecksumScanOn::Index => ScanOn::Index,
        };
        Scanner::new(&self.store, scan_on, false, false, range).map_err(Error::from)
    }
}

fn checksum_crc64_xor(checksum: u64, k: &[u8], v: &[u8]) -> u64 {
    let mut digest = Digest::new(crc64::ECMA);
    digest.write(k);
    digest.write(v);
    checksum ^ digest.sum64()
}
