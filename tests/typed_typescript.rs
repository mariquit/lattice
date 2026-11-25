// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

use lattice::ingest::{parse_typescript_file, resolve_with_root, CallEvidence};

#[test]
fn resolves_only_unambiguous_typed_this_fields() {
    let checkout = parse_typescript_file("checkout.ts", "\
import { Remote as Service } from './remote';
class Local { submit() {} }
class Other { submit() {} }
class Ambiguous { submit() {} submit() {} }
class A {
    private readonly svc!: Local;
    run() { this.svc.submit(); }
}
class B {
    constructor(private readonly svc: Service, ignored: Local) {}
    run() { this.svc.submit(); this.ignored.submit(); }
}
class C {
    svc!: Local | Other;
    duplicate!: Ambiguous;
    run() { this.svc.submit(); this.duplicate.submit(); }
}
class D {
    svc!: Local;
    static run() { this.svc.submit(); }
    nested() { function inner() { this.svc.submit(); } }
    another() { this.svc.submit(); }
}
class Generic<Local> {
    svc!: Local;
    run() { this.svc.submit(); }
}
").unwrap();
    let remote = parse_typescript_file("remote.ts", "export class Remote { submit() {} }").unwrap();
    let root = tempfile::tempdir().unwrap();
    let graph = resolve_with_root(&[checkout, remote], root.path().to_str().unwrap());
    let mut edges: Vec<_> = graph.calls.iter().map(|(from, to, evidence, site)| {
        assert_eq!(site.path, "checkout.ts");
        (from.as_str(), to.as_str(), *evidence, site.start_line, site.end_line)
    }).collect();
    edges.sort_by_key(|edge| edge.0);
    assert_eq!(edges, [
        ("checkout.ts::A::run", "checkout.ts::Local::submit", CallEvidence::Binding, 7, 7),
        ("checkout.ts::B::run", "remote.ts::Remote::submit", CallEvidence::Binding, 11, 11),
        ("checkout.ts::D::another", "checkout.ts::Local::submit", CallEvidence::Binding, 22, 22),
    ]);
}
