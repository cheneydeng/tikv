// Copyright 2017 PingCAP, Inc.
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

use std::sync::Arc;

use grpc::{ChannelBuilder, Environment};
use tikv::util::HandyRwLock;

use kvproto::tikvpb_grpc::TikvClient;
use kvproto::kvrpcpb::*;
use kvproto::raft_serverpb::*;
use futures::{Future, Sink};
use kvproto::coprocessor::*;

use super::server::*;
use super::cluster::Cluster;

fn must_new_cluster_and_client() -> (Cluster<ServerCluster>, TikvClient, Context) {
    let count = 1;
    let mut cluster = new_server_cluster(0, count);
    cluster.run();

    let region_id = 1;
    let leader = cluster.leader_of_region(region_id).unwrap();
    let epoch = cluster.get_region_epoch(region_id);
    let mut ctx = Context::new();
    ctx.set_region_id(region_id);
    ctx.set_peer(leader.clone());
    ctx.set_region_epoch(epoch);

    let addr = cluster.sim.rl().get_addr(leader.get_store_id());
    let env = Arc::new(Environment::new(1));
    let channel = ChannelBuilder::new(env).connect(&format!("{}", addr));
    let client = TikvClient::new(channel);

    (cluster, client, ctx)
}

#[test]
fn test_rawkv() {
    let (_cluster, client, ctx) = must_new_cluster_and_client();
    let (k, v) = (b"key".to_vec(), b"value".to_vec());

    // Raw put
    let mut put_req = RawPutRequest::new();
    put_req.set_context(ctx.clone());
    put_req.key = k.clone();
    put_req.value = v.clone();
    let put_resp = client.raw_put(put_req).unwrap();
    assert!(!put_resp.has_region_error());
    assert!(put_resp.error.is_empty());

    // Raw get
    let mut get_req = RawGetRequest::new();
    get_req.set_context(ctx.clone());
    get_req.key = k.clone();
    let get_resp = client.raw_get(get_req).unwrap();
    assert!(!get_resp.has_region_error());
    assert!(get_resp.error.is_empty());
    assert_eq!(get_resp.value, v);

    // Raw scan
    let mut scan_req = RawScanRequest::new();
    scan_req.set_context(ctx.clone());
    scan_req.start_key = k.clone();
    scan_req.limit = 1;
    let scan_resp = client.raw_scan(scan_req).unwrap();
    assert!(!scan_resp.has_region_error());
    assert_eq!(scan_resp.kvs.len(), 1);
    for kv in scan_resp.kvs.into_iter() {
        assert!(!kv.has_error());
        assert_eq!(kv.key, k);
        assert_eq!(kv.value, v);
    }

    // Raw delete
    let mut delete_req = RawDeleteRequest::new();
    delete_req.set_context(ctx.clone());
    delete_req.key = k.clone();
    let delete_resp = client.raw_delete(delete_req).unwrap();
    assert!(!delete_resp.has_region_error());
    assert!(delete_resp.error.is_empty());
}

fn must_kv_prewrite(client: &TikvClient, ctx: Context, muts: Vec<Mutation>, pk: Vec<u8>, ts: u64) {
    let mut prewrite_req = PrewriteRequest::new();
    prewrite_req.set_context(ctx);
    prewrite_req.set_mutations(muts.into_iter().collect());
    prewrite_req.primary_lock = pk;
    prewrite_req.start_version = ts;
    prewrite_req.lock_ttl = prewrite_req.start_version + 1;
    let prewrite_resp = client.kv_prewrite(prewrite_req).unwrap();
    assert!(
        !prewrite_resp.has_region_error(),
        "{:?}",
        prewrite_resp.get_region_error()
    );
    assert!(
        prewrite_resp.errors.is_empty(),
        "{:?}",
        prewrite_resp.get_errors()
    );
}

fn must_kv_commit(
    client: &TikvClient,
    ctx: Context,
    keys: Vec<Vec<u8>>,
    start_ts: u64,
    commit_ts: u64,
) {
    let mut commit_req = CommitRequest::new();
    commit_req.set_context(ctx);
    commit_req.start_version = start_ts;
    commit_req.set_keys(keys.into_iter().collect());
    commit_req.commit_version = commit_ts;
    let commit_resp = client.kv_commit(commit_req).unwrap();
    assert!(
        !commit_resp.has_region_error(),
        "{:?}",
        commit_resp.get_region_error()
    );
    assert!(!commit_resp.has_error(), "{:?}", commit_resp.get_error());
}

#[test]
fn test_mvcc_basic() {
    let (_cluster, client, ctx) = must_new_cluster_and_client();
    let (k, v) = (b"key".to_vec(), b"value".to_vec());

    let mut ts = 0;

    // Prewrite
    ts += 1;
    let prewrite_start_version = ts;
    let mut mutation = Mutation::new();
    mutation.op = Op::Put;
    mutation.key = k.clone();
    mutation.value = v.clone();
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![mutation],
        k.clone(),
        prewrite_start_version,
    );

    // Commit
    ts += 1;
    let commit_version = ts;
    must_kv_commit(
        &client,
        ctx.clone(),
        vec![k.clone()],
        prewrite_start_version,
        commit_version,
    );

    // Get
    ts += 1;
    let get_version = ts;
    let mut get_req = GetRequest::new();
    get_req.set_context(ctx.clone());
    get_req.key = k.clone();
    get_req.version = get_version;
    let get_resp = client.kv_get(get_req).unwrap();
    assert!(!get_resp.has_region_error());
    assert!(!get_resp.has_error());
    assert_eq!(get_resp.value, v);

    // Scan
    ts += 1;
    let scan_version = ts;
    let mut scan_req = ScanRequest::new();
    scan_req.set_context(ctx.clone());
    scan_req.start_key = k.clone();
    scan_req.limit = 1;
    scan_req.version = scan_version;
    let scan_resp = client.kv_scan(scan_req).unwrap();
    assert!(!scan_resp.has_region_error());
    assert_eq!(scan_resp.pairs.len(), 1);
    for kv in scan_resp.pairs.into_iter() {
        assert!(!kv.has_error());
        assert_eq!(kv.key, k);
        assert_eq!(kv.value, v);
    }

    // Batch get
    ts += 1;
    let batch_get_version = ts;
    let mut batch_get_req = BatchGetRequest::new();
    batch_get_req.set_context(ctx.clone());
    batch_get_req.set_keys(vec![k.clone()].into_iter().collect());
    batch_get_req.version = batch_get_version;
    let batch_get_resp = client.kv_batch_get(batch_get_req).unwrap();
    assert_eq!(batch_get_resp.pairs.len(), 1);
    for kv in batch_get_resp.pairs.into_iter() {
        assert!(!kv.has_error());
        assert_eq!(kv.key, k);
        assert_eq!(kv.value, v);
    }
}

#[test]
fn test_mvcc_rollback_and_cleanup() {
    let (_cluster, client, ctx) = must_new_cluster_and_client();
    let (k, v) = (b"key".to_vec(), b"value".to_vec());

    let mut ts = 0;

    // Prewrite
    ts += 1;
    let prewrite_start_version = ts;
    let mut mutation = Mutation::new();
    mutation.op = Op::Put;
    mutation.key = k.clone();
    mutation.value = v.clone();
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![mutation],
        k.clone(),
        prewrite_start_version,
    );

    // Commit
    ts += 1;
    let commit_version = ts;
    must_kv_commit(
        &client,
        ctx.clone(),
        vec![k.clone()],
        prewrite_start_version,
        commit_version,
    );

    // Prewrite puts some locks.
    ts += 1;
    let prewrite_start_version2 = ts;
    let (k2, v2) = (b"key2".to_vec(), b"value2".to_vec());
    let mut mut_pri = Mutation::new();
    mut_pri.op = Op::Put;
    mut_pri.key = k2.clone();
    mut_pri.value = v2.clone();
    let mut mut_sec = Mutation::new();
    mut_sec.op = Op::Put;
    mut_sec.key = k.clone();
    mut_sec.value = b"foo".to_vec();
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![mut_pri, mut_sec],
        k2.clone(),
        prewrite_start_version2,
    );

    // Scan lock, expects locks
    ts += 1;
    let scan_lock_max_version = ts;
    let mut scan_lock_req = ScanLockRequest::new();
    scan_lock_req.set_context(ctx.clone());
    scan_lock_req.max_version = scan_lock_max_version;
    let scan_lock_resp = client.kv_scan_lock(scan_lock_req).unwrap();
    assert!(!scan_lock_resp.has_region_error());
    assert_eq!(scan_lock_resp.locks.len(), 2);
    for (lock, key) in scan_lock_resp
        .locks
        .into_iter()
        .zip(vec![k.clone(), k2.clone()])
    {
        assert_eq!(lock.primary_lock, k2);
        assert_eq!(lock.key, key);
        assert_eq!(lock.lock_version, prewrite_start_version2);
    }

    // Rollback
    let rollback_start_version = prewrite_start_version2;
    let mut rollback_req = BatchRollbackRequest::new();
    rollback_req.set_context(ctx.clone());
    rollback_req.start_version = rollback_start_version;
    rollback_req.set_keys(vec![k2.clone()].into_iter().collect());
    let rollback_resp = client.kv_batch_rollback(rollback_req.clone()).unwrap();
    assert!(!rollback_resp.has_region_error());
    assert!(!rollback_resp.has_error());
    rollback_req.set_keys(vec![k.clone()].into_iter().collect());
    let rollback_resp2 = client.kv_batch_rollback(rollback_req.clone()).unwrap();
    assert!(!rollback_resp2.has_region_error());
    assert!(!rollback_resp2.has_error());

    // Cleanup
    let cleanup_start_version = prewrite_start_version2;
    let mut cleanup_req = CleanupRequest::new();
    cleanup_req.set_context(ctx.clone());
    cleanup_req.start_version = cleanup_start_version;
    cleanup_req.set_key(k2.clone());
    let cleanup_resp = client.kv_cleanup(cleanup_req).unwrap();
    assert!(!cleanup_resp.has_region_error());
    assert!(!cleanup_resp.has_error());

    // There should be no locks
    ts += 1;
    let scan_lock_max_version2 = ts;
    let mut scan_lock_req = ScanLockRequest::new();
    scan_lock_req.set_context(ctx.clone());
    scan_lock_req.max_version = scan_lock_max_version2;
    let scan_lock_resp = client.kv_scan_lock(scan_lock_req).unwrap();
    assert!(!scan_lock_resp.has_region_error());
    assert_eq!(scan_lock_resp.locks.len(), 0);
}

#[test]
fn test_mvcc_resolve_lock_gc_and_delete() {
    let (_cluster, client, ctx) = must_new_cluster_and_client();
    let (k, v) = (b"key".to_vec(), b"value".to_vec());

    let mut ts = 0;

    // Prewrite
    ts += 1;
    let prewrite_start_version = ts;
    let mut mutation = Mutation::new();
    mutation.op = Op::Put;
    mutation.key = k.clone();
    mutation.value = v.clone();
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![mutation],
        k.clone(),
        prewrite_start_version,
    );

    // Commit
    ts += 1;
    let commit_version = ts;
    must_kv_commit(
        &client,
        ctx.clone(),
        vec![k.clone()],
        prewrite_start_version,
        commit_version,
    );

    // Prewrite puts some locks.
    ts += 1;
    let prewrite_start_version2 = ts;
    let (k2, v2) = (b"key2".to_vec(), b"value2".to_vec());
    let new_v = b"new value".to_vec();
    let mut mut_pri = Mutation::new();
    mut_pri.op = Op::Put;
    mut_pri.key = k.clone();
    mut_pri.value = new_v.clone();
    let mut mut_sec = Mutation::new();
    mut_sec.op = Op::Put;
    mut_sec.key = k2.clone();
    mut_sec.value = v2.to_vec();
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![mut_pri, mut_sec],
        k.clone(),
        prewrite_start_version2,
    );

    // Resolve lock
    ts += 1;
    let resolve_lock_commit_version = ts;
    let mut resolve_lock_req = ResolveLockRequest::new();
    resolve_lock_req.set_context(ctx.clone());
    resolve_lock_req.start_version = prewrite_start_version2;
    resolve_lock_req.commit_version = resolve_lock_commit_version;
    let resolve_lock_resp = client.kv_resolve_lock(resolve_lock_req).unwrap();
    assert!(!resolve_lock_resp.has_region_error());
    assert!(!resolve_lock_resp.has_error());

    // Get `k` at the latest ts.
    ts += 1;
    let get_version1 = ts;
    let mut get_req1 = GetRequest::new();
    get_req1.set_context(ctx.clone());
    get_req1.key = k.clone();
    get_req1.version = get_version1;
    let get_resp1 = client.kv_get(get_req1).unwrap();
    assert!(!get_resp1.has_region_error());
    assert!(!get_resp1.has_error());
    assert_eq!(get_resp1.value, new_v);

    // GC `k` at the latest ts.
    ts += 1;
    let gc_safe_ponit = ts;
    let mut gc_req = GCRequest::new();
    gc_req.set_context(ctx.clone());
    gc_req.safe_point = gc_safe_ponit;
    let gc_resp = client.kv_gc(gc_req).unwrap();
    assert!(!gc_resp.has_region_error());
    assert!(!gc_resp.has_error());

    // the `k` at the old ts should be none.
    let get_version2 = commit_version + 1;
    let mut get_req2 = GetRequest::new();
    get_req2.set_context(ctx.clone());
    get_req2.key = k.clone();
    get_req2.version = get_version2;
    let get_resp2 = client.kv_get(get_req2).unwrap();
    assert!(!get_resp2.has_region_error());
    assert!(!get_resp2.has_error());
    assert_eq!(get_resp2.value, b"".to_vec());

    // Transaction debugger commands
    // MvccGetByKey
    let mut mvcc_get_by_key_req = MvccGetByKeyRequest::new();
    mvcc_get_by_key_req.set_context(ctx.clone());
    mvcc_get_by_key_req.key = k.clone();
    let mvcc_get_by_key_resp = client.mvcc_get_by_key(mvcc_get_by_key_req).unwrap();
    assert!(!mvcc_get_by_key_resp.has_region_error());
    assert!(mvcc_get_by_key_resp.error.is_empty());
    assert!(mvcc_get_by_key_resp.has_info());
    // MvccGetByStartTs
    let mut mvcc_get_by_start_ts_req = MvccGetByStartTsRequest::new();
    mvcc_get_by_start_ts_req.set_context(ctx.clone());
    mvcc_get_by_start_ts_req.start_ts = prewrite_start_version2;
    let mvcc_get_by_start_ts_resp = client
        .mvcc_get_by_start_ts(mvcc_get_by_start_ts_req)
        .unwrap();
    assert!(!mvcc_get_by_start_ts_resp.has_region_error());
    assert!(mvcc_get_by_start_ts_resp.error.is_empty());
    assert!(mvcc_get_by_start_ts_resp.has_info());
    assert_eq!(mvcc_get_by_start_ts_resp.key, k);

    // Delete range
    let mut del_req = DeleteRangeRequest::new();
    del_req.set_context(ctx.clone());
    del_req.start_key = b"a".to_vec();
    del_req.end_key = b"z".to_vec();
    let del_resp = client.kv_delete_range(del_req).unwrap();
    assert!(!del_resp.has_region_error());
    assert!(del_resp.error.is_empty());
}

#[test]
fn test_raft() {
    let (_cluster, client, _) = must_new_cluster_and_client();

    // Raft commands
    let (sink, _) = client.raft();
    sink.send((RaftMessage::new(), Default::default()))
        .wait()
        .unwrap();

    let (sink, _) = client.snapshot();
    sink.send((SnapshotChunk::new(), Default::default()))
        .wait()
        .unwrap();
}

#[test]
fn test_coprocessor() {
    let (_cluster, client, _) = must_new_cluster_and_client();

    // SQL push down commands
    client.coprocessor(Request::new()).unwrap();
}
