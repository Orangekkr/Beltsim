// Catalog-law tests. Each test maps to a law or bench expectation from the
// certified parts catalog. These are the contract the Stage 1 event core
// must also pass, plus differential equivalence against this oracle.

use beltsim::netlist::parse;
use beltsim::sim::TICK_RATE;


// Inlined circuit fixtures: tests are self-contained and do not depend
// on the examples folder (which is free to change).
const POISON_SEAL: &str = r#"
item one
item zero
node PAY source item=one rate=270/min
node PILL source item=zero rate=60/min limit=1 start=5
node M merger
node SS smartsplitter
node K sink
edge A PAY -> M.in0 mk=3 slots=6
edge B PILL -> M.in1 mk=1 slots=2
edge C M.out -> SS.in mk=3 slots=8
edge D SS.out0 -> K.in mk=3 slots=6 rule=one
probe C
"#;

const ONE_SLOT_STUB: &str = r#"
item one
item zero
node SRC source item=one rate=120/min limit=3
node SP splitter
node JAM smartsplitter
node OVF sink
node NEVER sink
edge FEED SRC -> SP.in mk=2 slots=4
edge STUB SP.out0 -> JAM.in mk=1 slots=1
edge OVERFLOW SP.out1 -> OVF.in mk=2 slots=4
edge NEVERLANE JAM.out0 -> NEVER.in mk=1 slots=2 rule=zero
probe STUB
"#;

const BLOCKER_VALVE: &str = r#"
item one
item zero
node CTRL source item=zero rate=120/min stop=20
node PAY source item=one rate=270/min
node V pmerger
node STRIP smartsplitter
node LEAK sink
node OUT sink
edge C CTRL -> V.high mk=2 slots=3 prefill=zero:3
edge P PAY -> V.low mk=3 slots=6
edge D V.out -> STRIP.in mk=1 slots=2
edge L STRIP.out0 -> LEAK.in mk=1 slots=2 rule=zero
edge O STRIP.out1 -> OUT.in mk=3 slots=2 rule=one
probe D
probe O
"#;

const SMOOTHER: &str = r#"
item one
node SRC source item=one rate=480/min
node BUF container cap=400
node K sink
edge A SRC -> BUF.in mk=4 slots=8
edge B BUF.out -> K.in mk=3 slots=8
probe B
"#;

const SEAL_FLUSH: &str = r#"
item one
item zero
node PAY source item=one rate=270/min
node PILL source item=zero rate=60/min limit=1 start=5
node M merger
node SS smartsplitter
node K sink
edge A PAY -> M.in0 mk=3 slots=6
edge B PILL -> M.in1 mk=1 slots=2
edge C M.out -> SS.in mk=3 slots=8
edge D SS.out0 -> K.in mk=3 slots=6 rule=one
display FLOW counter K item=one
display PILLS counter K item=zero
at 12 rule SS.out0 one,zero
probe C
"#;

fn secs(s: f64) -> u64 {
    (s * TICK_RATE as f64).round() as u64
}

// LAW: strict priority. High saturated means out carries only High items.
// A backed-up Low never blocks High.
#[test]
fn test_priority_merger_starvation() {
    let nl = "
item one
item zero
node H source item=zero rate=270/min
node L source item=one rate=270/min
node P pmerger
node K sink
edge Eh H -> P.high mk=3 slots=6
edge El L -> P.low mk=3 slots=6
edge Eo P.out -> K.in mk=3 slots=6
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(30.0));
    let zeros = w.sink_count("K", "zero");
    let ones = w.sink_count("K", "one");
    assert!(zeros > 100, "high stream should flow, got {}", zeros);
    // Central 1-item buffer may admit at most one low item at startup
    // before the first high arrival (PM-Low Leak at the initial gap).
    assert!(ones <= 1, "low must be starved, leaked {}", ones);
    // Low side fully backed up: source edge saturated.
    assert_eq!(w.edge_count("El"), 6, "low input should be backed up");
}

// LAW: merge law. Output blocked means BOTH inputs back up.
#[test]
fn test_merger_backpressure() {
    let nl = "
item one
node A source item=one rate=120/min
node B source item=one rate=120/min
node M merger
node G gate initial=closed
node K sink
edge Ea A -> M.in0 mk=2 slots=4
edge Eb B -> M.in1 mk=2 slots=4
edge Eo M.out -> G.in mk=2 slots=3
edge Ed G.out -> K.in mk=2 slots=3
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.edge_count("Ea"), 4, "input A must back up");
    assert_eq!(w.edge_count("Eb"), 4, "input B must back up");
    assert_eq!(w.edge_count("Eo"), 3, "merger output must fill");
    assert_eq!(w.sink_count("K", "one"), 0, "gate closed, nothing passes");
}

// LAW: splitter deals round-robin and skips blocked ports without losing
// rhythm on the open ones. One output dead-ends into a jam; the other two
// keep receiving an exact 50/50 split of subsequent items.
#[test]
fn test_splitter_rr_skip() {
    let nl = "
item one
item zero
node S source item=one rate=480/min
node SP splitter
node JAM smartsplitter
node K1 sink
node K2 sink
edge Ein S -> SP.in mk=4 slots=6
edge E0 SP.out0 -> JAM.in mk=4 slots=2
edge Ej JAM.out0 -> K1.in mk=1 slots=2 rule=zero
edge E1 SP.out1 -> K2.in mk=4 slots=6
";
    // out0 jams after 2 slots + 1 port buffer absorb 3 items.
    // Everything after that goes to out1. Counts must stay exact.
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(30.0));
    let emitted: u64 = 30 * 8; // 480/min = 8/s for 30 s
    let stuck = w.edge_count("E0") as u64 + w.node_buffered("SP") as u64;
    let delivered = w.sink_count("K2", "one");
    let in_flight = w.edge_count("Ein") as u64 + w.edge_count("E1") as u64;
    assert_eq!(w.edge_count("E0"), 2, "jammed lane holds its 2 slots");
    assert_eq!(
        emitted,
        stuck + delivered + in_flight,
        "count conservation must be exact"
    );
    assert!(delivered > 200, "open lane must keep flowing, got {}", delivered);
}

// LAW: poison pill. One unroutable item at a smart splitter head halts ALL
// types behind it. Faults manifest as stalls, not corruption.
#[test]
fn test_poison_pill() {
    let text = POISON_SEAL.to_string();
    let mut w = parse(&text).unwrap();
    w.run_ticks(secs(10.0));
    let at_10 = w.sink_count("K", "one");
    w.run_ticks(secs(10.0));
    let at_20 = w.sink_count("K", "one");
    assert!(at_10 > 0, "payload must flow before the pill lands");
    assert_eq!(at_10, at_20, "sink count must freeze after the pill");
    assert_eq!(w.edge_count("C"), 8, "belt behind the pill must fill");
}

// BENCH: one-slot stub end state. Feed exactly 3: one parks in the lift,
// one is held in the entry splitter's port buffer, one overflows.
// Counts exact; order may differ from in-game.
#[test]
fn test_one_slot_stub() {
    let text = ONE_SLOT_STUB.to_string();
    let mut w = parse(&text).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.edge_count("STUB"), 1, "exactly one item parks in the stub");
    assert_eq!(
        w.node_buffered("SP"),
        1,
        "exactly one item held in the splitter port buffer"
    );
    assert_eq!(
        w.sink_count("OVF", "one"),
        1,
        "exactly one item overflows to OVF"
    );
}

// LAW: Colander. A PM whose High refill rate exceeds the out drain rate
// holds a jam with ZERO leak-through of the Low type. Cutting control
// opens the valve within a bounded, measurable time.
#[test]
fn test_blocker_valve() {
    let text = BLOCKER_VALVE.to_string();
    let mut w = parse(&text).unwrap();
    // Closed phase: run to t=20s (control cut happens at 20).
    w.run_ticks(secs(20.0));
    assert_eq!(
        w.sink_count("OUT", "one"),
        0,
        "valve closed: zero payload leak-through"
    );
    assert!(
        w.sink_count("LEAK", "zero") > 10,
        "control burn must drain at the leak"
    );
    // Open latency arithmetic for THIS valve variant: 3 queued control
    // zeros drain through the Mk1 leak at 1/s (~3 s), then payload transits
    // D (Mk1, ~1-2 s) plus the fast observer edge. Earliest possible open
    // is ~5 s after the cut. Bracket it from both sides:
    // still closed at cut+2 s, open by cut+10 s (one-sided margins).
    w.run_ticks(secs(2.0));
    assert_eq!(
        w.sink_count("OUT", "one"),
        0,
        "queue must hold the valve closed for at least 2 s after the cut"
    );
    w.run_ticks(secs(8.0));
    assert!(
        w.sink_count("OUT", "one") > 0,
        "valve must open within 10 s of control cut"
    );
}

// BENCH: smoother. Container fed at 8/s draining over Mk3 produces a
// gapless output: sink inter-arrival is exactly one Mk3 period (1040
// ticks) in steady state.
#[test]
fn test_smoother_gapless() {
    let text = SMOOTHER.to_string();
    let mut w = parse(&text).unwrap();
    // Warm up 10 s, then measure arrivals over exactly 20 s.
    w.run_ticks(secs(10.0));
    let before = w.sink_count("K", "one");
    w.run_ticks(secs(20.0));
    let after = w.sink_count("K", "one");
    let arrivals = after - before;
    // Mk3 is 4.5 items/s: 20 s = exactly 90 items, gapless.
    assert_eq!(arrivals, 90, "gapless Mk3 drain must deliver exactly 90 in 20 s");
}

// LAW: containers preserve type and count (order and timing destroyed).
#[test]
fn test_container_count_preserved() {
    let nl = "
item one
node S source item=one rate=270/min limit=100
node C container cap=1000
node K sink
edge A S -> C.in mk=3 slots=4
edge B C.out -> K.in mk=3 slots=4
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(60.0));
    assert_eq!(
        w.sink_count("K", "one"),
        100,
        "every emitted item must arrive, no more, no less"
    );
}

// Determinism: identical netlist, identical tick count, identical full
// state hash. Twice.
#[test]
fn test_determinism() {
    let text = BLOCKER_VALVE.to_string();
    let mut a = parse(&text).unwrap();
    let mut b = parse(&text).unwrap();
    a.run_ticks(secs(37.5));
    b.run_ticks(secs(37.5));
    assert_eq!(a.state_hash(), b.state_hash(), "same input, same state, always");
}

// PM-Low Leak Law: a Low storage parked at a PM leaks exactly one wrong
// item per High gap. We create one deliberate gap in the High stream and
// count the leakage.
#[test]
fn test_pm_low_leak_one_per_gap() {
    // High: zeros at 2/s but with a hole: source stops at t=10, resumes via
    // a second source at t=12 (one gap). Low: saturated ones.
    let nl = "
item one
item zero
node H1 source item=zero rate=120/min stop=10
node H2 source item=zero rate=120/min start=12
node HM merger
node L source item=one rate=270/min
node P pmerger
node SS smartsplitter
node KZ sink
node KO sink
edge Eh1 H1 -> HM.in0 mk=2 slots=2
edge Eh2 H2 -> HM.in1 mk=2 slots=2
edge Eh HM.out -> P.high mk=2 slots=2
edge El L -> P.low mk=3 slots=6
edge Eo P.out -> SS.in mk=1 slots=2
edge Ez SS.out0 -> KZ.in mk=1 slots=2 rule=zero
edge Eo1 SS.out1 -> KO.in mk=1 slots=4 rule=one
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(30.0));
    let leaked = w.sink_count("KO", "one");
    // Exactly one gap in High (plus the initial startup gap before the
    // first zero lands). Leak must be small and bounded: >= 1 (the law
    // says it leaks) and <= 3 (one per gap, two gaps max, plus none extra).
    assert!(leaked >= 1, "PM must leak during a High gap, got {}", leaked);
    assert!(leaked <= 3, "leak must be bounded at one per gap, got {}", leaked);
}

// ---------- v0.2 feature tests ----------

// FEATURE: multi-rule ports. One port carrying two exact filters accepts
// both item types.
#[test]
fn test_multi_rule_port() {
    let nl = "
item one
item zero
node S1 source item=one rate=270/min
node S0 source item=zero rate=60/min
node M merger
node SS smartsplitter
node K sink
edge A S1 -> M.in0 mk=3 slots=4
edge B S0 -> M.in1 mk=1 slots=4
edge C M.out -> SS.in mk=3 slots=6
edge D SS.out0 -> K.in mk=3 slots=6 rule=one,zero
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert!(w.sink_count("K", "one") > 50, "ones flow through shared port");
    assert!(w.sink_count("K", "zero") > 10, "zeros flow through shared port");
}

// SEMANTICS (pre-bench): `any` matches everything and COMPETES with exact
// filters. Ones round-robin between their exact port and the any port;
// zeros only ever use the any port.
#[test]
fn test_any_competes_with_exact() {
    let nl = "
item one
item zero
node S1 source item=one rate=270/min
node S0 source item=zero rate=60/min
node M merger
node SS smartsplitter
node K0 sink
node K1 sink
edge A S1 -> M.in0 mk=3 slots=4
edge B S0 -> M.in1 mk=1 slots=4
edge C M.out -> SS.in mk=3 slots=6
edge D0 SS.out0 -> K0.in mk=3 slots=6 rule=one
edge D1 SS.out1 -> K1.in mk=3 slots=6 rule=any
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    let ones_total = w.sink_count("K0", "one") + w.sink_count("K1", "one");
    assert!(
        w.sink_count("K0", "one") > ones_total / 4,
        "exact port gets a real share of ones"
    );
    assert!(
        w.sink_count("K1", "one") > ones_total / 4,
        "any port competes for ones"
    );
    assert_eq!(w.sink_count("K0", "zero"), 0, "zeros never match the exact port");
    assert!(w.sink_count("K1", "zero") > 10, "zeros use the any port");
}

// SEMANTICS (pre-bench): `undefined` matches an item only when no port on
// the splitter carries an exact filter for it. Ones (which have an exact
// port) never touch the undefined port.
#[test]
fn test_undefined_only_without_exact() {
    let nl = "
item one
item zero
node S1 source item=one rate=270/min
node S0 source item=zero rate=60/min
node M merger
node SS smartsplitter
node K0 sink
node K1 sink
edge A S1 -> M.in0 mk=3 slots=4
edge B S0 -> M.in1 mk=1 slots=4
edge C M.out -> SS.in mk=3 slots=6
edge D0 SS.out0 -> K0.in mk=3 slots=6 rule=one
edge D1 SS.out1 -> K1.in mk=3 slots=6 rule=undefined
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert!(w.sink_count("K0", "one") > 50, "ones use their exact port");
    assert_eq!(w.sink_count("K1", "one"), 0, "undefined ignores exactly-filtered items");
    assert_eq!(w.sink_count("K0", "zero"), 0);
    assert!(w.sink_count("K1", "zero") > 10, "unfiltered items use undefined");
}

// SEMANTICS (pre-bench): `overflow` is the last resort. Items go there only
// once the eligible port cannot accept: its lane and buffer absorb exactly
// buffer(1) + slots(2) = 3 items, everything after overflows.
#[test]
fn test_overflow_last_resort() {
    let nl = "
item one
node S source item=one rate=270/min
node SS smartsplitter
node G gate initial=closed
node KX sink
node K1 sink
edge C S -> SS.in mk=3 slots=6
edge LANE SS.out0 -> G.in mk=3 slots=2 rule=one
edge GX G.out -> KX.in mk=3 slots=2
edge D1 SS.out1 -> K1.in mk=3 slots=6 rule=overflow
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.edge_count("LANE"), 2, "primary lane saturates");
    assert_eq!(w.node_buffered("SS"), 1, "primary port buffer holds one");
    assert_eq!(w.sink_count("KX", "one"), 0, "closed gate passes nothing");
    assert!(
        w.sink_count("K1", "one") >= 60,
        "everything past the absorbed 3 overflows, got {}",
        w.sink_count("K1", "one")
    );
}

// FEATURE: runtime rule edits. A poison-sealed splitter is flushed by a
// scripted rule change: flow is frozen before the edit and resumes after,
// and the single pill exits through the widened port.
#[test]
fn test_runtime_rule_flush() {
    let text = SEAL_FLUSH;
    let mut w = parse(text).unwrap();
    w.run_ticks(secs(10.5));
    let frozen_a = w.sink_count("K", "one");
    w.run_ticks(secs(1.0));
    let frozen_b = w.sink_count("K", "one");
    assert_eq!(frozen_a, frozen_b, "seal holds before the rule edit");
    w.run_ticks(secs(8.5));
    assert!(
        w.sink_count("K", "one") > frozen_b + 10,
        "flow resumes after the flush"
    );
    assert_eq!(w.sink_count("K", "zero"), 1, "the pill drained through");
}

// FEATURE: runtime priority remap. After swapping high and low tiers, the
// previously starved input flows and the previously favored one starves
// (modulo the one-item buffer leak).
#[test]
fn test_priority_swap() {
    let nl = "
item one
item zero
node SA source item=one rate=270/min
node SB source item=zero rate=270/min
node P pmerger
node K sink
edge Ea SA -> P.high mk=3 slots=4
edge Eb SB -> P.low mk=3 slots=4
edge Eo P.out -> K.in mk=3 slots=4
at 10 priority P high=Eb low=Ea
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(10.0));
    let ones_mid = w.sink_count("K", "one");
    let zeros_mid = w.sink_count("K", "zero");
    assert!(ones_mid > 35, "high side flows before the swap");
    assert!(zeros_mid <= 1, "low side starves before the swap");
    // Let the pre-swap pipeline (output belt + central buffer) drain, then
    // measure over a clean window: in-flight ones are drain, not leak.
    w.run_ticks(secs(2.0));
    let ones_drained = w.sink_count("K", "one");
    let zeros_drained = w.sink_count("K", "zero");
    w.run_ticks(secs(8.0));
    let ones_delta = w.sink_count("K", "one") - ones_drained;
    let zeros_delta = w.sink_count("K", "zero") - zeros_drained;
    assert!(zeros_delta > 30, "new high side flows, got {}", zeros_delta);
    assert!(ones_delta <= 1, "new low side starves, got {}", ones_delta);
    let _ = zeros_mid;
}

// FEATURE: buttons inject exact counts. One scripted press of 5 delivers
// exactly 5 items, no more, no fewer.
#[test]
fn test_button_exact_count() {
    let nl = "
item one
node B button item=one
node K sink
edge W B -> K.in mk=2 slots=4
at 2 press B 5
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.sink_count("K", "one"), 5, "press count is exact");
}

// QUIRK (pre-bench, bench 4): a lift's mouth is an extra entrance slot.
// A 1-slot lift with a mouth parks 2 tokens (minimum seal = 2); the tight
// splitter-port coupling (mouth=0) parks 1.
#[test]
fn test_lift_mouth_capacity() {
    let with_mouth = "
item one
item pill
node S source item=one rate=270/min limit=3
node CAP smartsplitter
node F sink
edge LIFT S -> CAP.in lift mk=1 slots=1
edge X CAP.out0 -> F.in mk=1 slots=2 rule=pill
";
    let mut w = parse(with_mouth).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.edges[w.edge_id("LIFT").unwrap()].cap(), 2);
    assert_eq!(w.edge_count("LIFT"), 2, "mouth plus slot park two tokens");

    let tight = "
item one
item pill
node S source item=one rate=270/min limit=3
node CAP smartsplitter
node F sink
edge LIFT S -> CAP.in lift mouth=0 mk=1 slots=1
edge X CAP.out0 -> F.in mk=1 slots=2 rule=pill
";
    let mut w2 = parse(tight).unwrap();
    w2.run_ticks(secs(20.0));
    assert_eq!(w2.edges[w2.edge_id("LIFT").unwrap()].cap(), 1);
    assert_eq!(w2.edge_count("LIFT"), 1, "tight coupling parks one token");
}

// FEATURE: register display decode. Saturated-one reads 1, saturated-zero
// reads 0, empty reads N; a register with any N is flagged unsettled, a
// fully settled register decodes to hex and decimal.
#[test]
fn test_register_display_decode() {
    let nl = "
item one
item zero
node S3 source item=one limit=0 rate=60/min
node S2 source item=one limit=0 rate=60/min
node S1 source item=one limit=0 rate=60/min
node S0 source item=one limit=0 rate=60/min
node G3 gate initial=closed
node G2 gate initial=closed
node G1 gate initial=closed
node G0 gate initial=closed
node K3 sink
node K2 sink
node K1 sink
node K0 sink
edge E3 S3 -> G3.in mk=1 slots=2 prefill=one:full
edge E2 S2 -> G2.in mk=1 slots=2 prefill=zero:full
edge E1 S1 -> G1.in mk=1 slots=2
edge E0 S0 -> G0.in mk=1 slots=2 prefill=one:full
edge Y3 G3.out -> K3.in mk=1 slots=2
edge Y2 G2.out -> K2.in mk=1 slots=2
edge Y1 G1.out -> K1.in mk=1 slots=2
edge Y0 G0.out -> K0.in mk=1 slots=2
display RA register E3,E2,E1,E0 one=one zero=zero
display RB register E3,E2,E0 one=one zero=zero
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(2.0));
    let panel = w.render_displays();
    assert!(
        panel.contains("10N1 (not settled)"),
        "unsettled register shows N, panel was:\n{}",
        panel
    );
    assert!(
        panel.contains("101 = 0x5 = 5"),
        "settled register decodes, panel was:\n{}",
        panel
    );
}

// LAW: determinism holds through scripted stimuli (rule edits, presses).
#[test]
fn test_determinism_with_stimuli() {
    let text = SEAL_FLUSH;
    let mut a = parse(text).unwrap();
    let mut b = parse(text).unwrap();
    a.run_ticks(secs(30.0));
    b.run_ticks(secs(30.0));
    assert_eq!(a.state_hash(), b.state_hash());
}

// ---------- v0.3 module tests ----------

use beltsim::netlist::parse_file;

// FEATURE: instantiation is pure inlining. A module version and its
// hand-expanded flat version are tick-identical (equal state hashes),
// because expansion adds zero entities and preserves declaration order.
#[test]
fn test_module_flat_equivalence() {
    let modular = "
item one
def half
node M merger
node K sink
edge O M.out -> K.in mk=3 slots=4
pin a M.in0
end
node S source item=one rate=270/min
inst h half
edge W S -> h.a mk=3 slots=4
";
    let flat = "
item one
node S source item=one rate=270/min
node M merger
node K sink
edge O M.out -> K.in mk=3 slots=4
edge W S -> M.in0 mk=3 slots=4
";
    let mut a = parse(modular).unwrap();
    let mut b = parse(flat).unwrap();
    a.run_ticks(secs(30.0));
    b.run_ticks(secs(30.0));
    assert_eq!(a.state_hash(), b.state_hash(), "expansion must be pure inlining");
    assert_eq!(a.sink_count("h/K", "one"), b.sink_count("K", "one"));
}

// FEATURE: nested instances and pin forwarding. An outer module re-exports
// an inner instance's pin; the caller wires through both layers.
#[test]
fn test_nested_modules_pin_forwarding() {
    let nl = "
item one
def inner
node K sink
pin i K.in
end
def outer
inst x inner
pin i x.i
end
node S source item=one rate=270/min limit=7
inst o outer
edge W S -> o.i mk=3 slots=4
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.sink_count("o/x/K", "one"), 7, "items reach the doubly-nested sink");
}

// SAFETY: recursive instantiation is a parse error, not a hang.
#[test]
fn test_recursion_rejected() {
    let nl = "
item one
def a
inst x a
end
inst top a
";
    let err = match parse(nl) {
        Err(e) => e,
        Ok(_) => panic!("expected a parse error"),
    };
    assert!(err.contains("recursive"), "got: {}", err);
}

// FEATURE: use imports definitions from another file, paths relative to the
// importing file. The shipped tutorial example exercises or2, longwire,
// parkbit, hierarchical stimuli, and displays.
#[test]
fn test_use_import_tutorial() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/seal_flush.blt");
    let mut w = parse_file(path).unwrap();
    w.run_ticks(secs(40.0));
    // Seal half: matches the SETUP.md verification promise.
    assert!(w.sink_count("K", "one") > 130, "FLOW around 140");
    assert_eq!(w.sink_count("K", "zero"), 1, "PILLS exactly 1");
    // Module half.
    assert_eq!(w.sink_count("KM", "one"), 3);
    assert_eq!(w.sink_count("KM", "zero"), 3);
    assert_eq!(w.sink_count("bit/F", "one"), 1, "flush drained the parked bit");
    assert_eq!(w.edge_count("CELL"), 0, "bit reads NULL after flush");
}

// SAFETY: wiring FROM an input pin is a port-direction error.
#[test]
fn test_pin_direction_error() {
    let nl = "
item one
def or2
node M merger
pin a M.in0
pin q M.out
end
node K sink
inst g or2
edge X g.a -> K.in mk=2 slots=2
";
    let err = match parse(nl) {
        Err(e) => e,
        Ok(_) => panic!("expected a parse error"),
    };
    assert!(err.contains("output port"), "got: {}", err);
}

// SAFETY: used files may only declare items and defs.
#[test]
fn test_used_file_restriction() {
    let dir = std::env::temp_dir();
    let lib = dir.join("beltsim_badlib_test.blt");
    let main = dir.join("beltsim_badmain_test.blt");
    std::fs::write(&lib, "item one\nnode X sink\n").unwrap();
    std::fs::write(
        &main,
        format!("use {}\nitem one\n", lib.file_name().unwrap().to_str().unwrap()),
    )
    .unwrap();
    let err = match parse_file(main.to_str().unwrap()) {
        Err(e) => e,
        Ok(_) => panic!("expected a parse error"),
    };
    assert!(err.contains("used files"), "got: {}", err);
}

// FEATURE: item declarations are idempotent (libraries re-declare freely).
#[test]
fn test_item_idempotent() {
    let nl = "
item one
item one
node S source item=one rate=270/min limit=2
node K sink
edge W S -> K.in mk=3 slots=4
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(10.0));
    assert_eq!(w.sink_count("K", "one"), 2);
}

// FEATURE: displays inside a def expand per instance with slash prefixes.
#[test]
fn test_display_prefixing() {
    let nl = "
item one
def probe1
node K sink
pin i K.in
display R counter K
end
node S1 source item=one rate=270/min limit=1
node S2 source item=one rate=270/min limit=2
inst a probe1
inst b probe1
edge W1 S1 -> a.i mk=3 slots=4
edge W2 S2 -> b.i mk=3 slots=4
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(10.0));
    let panel = w.render_displays();
    assert!(panel.contains("a/R"), "panel: {}", panel);
    assert!(panel.contains("b/R"), "panel: {}", panel);
    assert_eq!(w.sink_count("a/K", "one"), 1);
    assert_eq!(w.sink_count("b/K", "one"), 2);
}

// ---------- v0.3.1 optimization tests ----------

// EXACTNESS: tick skipping and edge sleeping must be invisible. Naive
// per-tick stepping and the skipping engine produce identical state hashes
// at several checkpoints across a circuit exercising stimuli, a poison jam,
// a runtime flush, probes, and displays.
#[test]
fn test_skip_differential() {
    let text = SEAL_FLUSH;
    let mut naive = parse(text).unwrap();
    naive.no_skip = true;
    let mut fast = parse(text).unwrap();
    for checkpoint in [5.0_f64, 10.5, 12.0, 13.0, 25.0, 60.0] {
        let target = secs(checkpoint);
        let n_now = naive.tick;
        let f_now = fast.tick;
        naive.run_ticks(target - n_now);
        fast.run_ticks(target - f_now);
        assert_eq!(naive.tick, fast.tick, "tick drift at {}s", checkpoint);
        assert_eq!(
            naive.state_hash(),
            fast.state_hash(),
            "state divergence at {}s",
            checkpoint
        );
    }
}

// EXACTNESS: run_until_quiet returns the same tick count with and without
// skipping (the quiet boundary is clamped, not overshot).
#[test]
fn test_skip_quiet_equivalence() {
    let text = ONE_SLOT_STUB;
    let mut naive = parse(text).unwrap();
    naive.no_skip = true;
    let mut fast = parse(text).unwrap();
    let a = naive.run_until_quiet(secs(120.0), 2 * TICK_RATE);
    let b = fast.run_until_quiet(secs(120.0), 2 * TICK_RATE);
    assert_eq!(a, b, "quiet stop tick must match");
    assert_eq!(naive.state_hash(), fast.state_hash());
}

// ---------- v0.4 defaults, aliases, nesting, meters ----------

// FEATURE: global defaults fill missing edge and source keys; explicit keys
// win; mk/rate are one exclusive group.
#[test]
fn test_defaults_global() {
    let nl = "
item one
default edge mk=6 slots=4
default source rate=1200/min item=one
node S source
node K sink
edge W S -> K.in
edge X2 K2S -> K2.in mk=1 slots=2
";
    // second edge exercises explicit override; needs its own nodes
    let nl = nl.replace("edge X2 K2S -> K2.in mk=1 slots=2",
        "node K2S source limit=0\nnode K2 sink\nedge X2 K2S -> K2.in mk=1 slots=2");
    let mut w = parse(&nl).unwrap();
    let wid = w.edge_id("W").unwrap();
    let xid = w.edge_id("X2").unwrap();
    assert_eq!(w.edges[wid].period, 234, "default mk=6");
    assert_eq!(w.edges[wid].slots.len(), 4, "default slots=4");
    assert_eq!(w.edges[xid].period, 4680, "explicit mk=1 beats default");
    w.run_ticks(secs(10.0));
    assert!(w.sink_count("K", "one") > 150, "source defaulted to 1200/min, got {}", w.sink_count("K", "one"));
}

// FEATURE: defaults are lexically scoped. A def carries the defaults from
// its definition site; caller-side defaults never leak into a module.
#[test]
fn test_defaults_scoped_and_hermetic() {
    let nl = "
item one
def slowpart
default edge mk=1 slots=1
node K sink
pin i K.in
end
default edge mk=6 slots=8
node S source item=one rate=1200/min
inst p slowpart
edge W S -> p.i
";
    // W is wired at top level (mk6 default); the module has no internal
    // edges here, so scoping is shown by a second module with one.
    let nl2 = "
item one
default edge mk=6 slots=8
def inner
default edge mk=1 slots=1
node G gate
node K sink
edge INSIDE G.out -> K.in
pin i G.in
end
node S source item=one rate=270/min
inst p inner
edge W S -> p.i
";
    let _ = nl;
    let w = parse(nl2).unwrap();
    let inside = w.edge_id("p/INSIDE").unwrap();
    let outside = w.edge_id("W").unwrap();
    assert_eq!(w.edges[inside].period, 4680, "module-local default mk=1 applies inside");
    assert_eq!(w.edges[outside].period, 234, "global default mk=6 applies at top level");
}

// FEATURE: a module with NO local default does not inherit the caller's
// instantiation-site defaults, only its definition-site ones.
#[test]
fn test_defaults_definition_site_capture() {
    let nl = "
item one
default edge mk=2 slots=3
def part
node G gate
node K sink
edge INSIDE G.out -> K.in
pin i G.in
end
default edge mk=6 slots=8
node S source item=one rate=270/min
inst p part
edge W S -> p.i
";
    let w = parse(nl).unwrap();
    // part was DEFINED while mk=2 was active, instantiated while mk=6 was.
    assert_eq!(w.edges[w.edge_id("p/INSIDE").unwrap()].period, 2340, "definition-site default (mk=2) captured");
    assert_eq!(w.edges[w.edge_id("W").unwrap()].period, 234, "instantiation site uses current default (mk=6)");
}

// FEATURE: aliases are whole-token substitutions, multi-token expansions
// allowed, scoped like defaults.
#[test]
fn test_aliases() {
    let nl = "
item one
item zero
alias fast mk=6 slots=6
alias rOv rule=overflow
alias r1 rule=one
node S source item=one rate=270/min
node SS smartsplitter
node G gate initial=closed
node KX sink
node K1 sink
edge C S -> SS.in fast
edge LANE SS.out0 -> G.in mk=3 slots=2 r1
edge GX G.out -> KX.in mk=3 slots=2
edge D1 SS.out1 -> K1.in fast rOv
";
    let mut w = parse(nl).unwrap();
    assert_eq!(w.edges[w.edge_id("C").unwrap()].period, 234);
    assert_eq!(w.edges[w.edge_id("C").unwrap()].slots.len(), 6);
    w.run_ticks(secs(20.0));
    assert!(w.sink_count("K1", "one") >= 60, "overflow alias worked, got {}", w.sink_count("K1", "one"));
    assert_eq!(w.edge_count("LANE"), 2);
}

// FEATURE: defs nest lexically. An inner module is visible inside its
// enclosing def and invisible outside it.
#[test]
fn test_nested_def_scoping() {
    let good = "
item one
def outer
def inner
node K sink
pin i K.in
end
inst x inner
pin i x.i
end
node S source item=one rate=270/min limit=4
inst o outer
edge W S -> o.i mk=3 slots=4
";
    let mut w = parse(good).unwrap();
    w.run_ticks(secs(20.0));
    assert_eq!(w.sink_count("o/x/K", "one"), 4);

    let bad = "
item one
def outer
def inner
node K sink
pin i K.in
end
inst x inner
pin i x.i
end
inst y inner
";
    let err = match parse(bad) {
        Err(e) => e,
        Ok(_) => panic!("inner must not be visible at top level"),
    };
    assert!(err.contains("unknown module inner"), "got: {}", err);
}

// FEATURE: meter display counts throughput; total equals what the sink
// consumed for an edge feeding a sink. Pin references resolve to the
// attached edge.
#[test]
fn test_meter_and_pin_ref() {
    let nl = "
item one
def part
node M merger
pin a M.in0
pin q M.out
end
node S source item=one rate=270/min limit=20
node K sink
inst g part
edge E S -> g.a mk=3 slots=4
edge W g.q -> K.in mk=3 slots=4
display MW meter W
display MP meter g.q
display MPIN lamp g.a
";
    let mut w = parse(nl).unwrap();
    w.run_ticks(secs(30.0));
    let panel = w.render_displays();
    assert!(panel.contains("MW"), "panel: {}", panel);
    assert!(panel.contains("total=20"), "meter total equals delivered, panel: {}", panel);
    // MP resolves g.q (an out pin) to edge W: same numbers.
    let mp_line: Vec<&str> = panel.lines().filter(|l| l.contains("MP ")).collect();
    assert!(mp_line[0].contains("total=20"), "pin meter: {}", mp_line[0]);
    assert_eq!(w.sink_count("K", "one"), 20);
}

// FEATURE: '>' is the edge arrow; '->' remains accepted (older tests above
// exercise it throughout). Both parse to the same machine.
#[test]
fn test_arrow_forms_equivalent() {
    let a = "
item one
node S source item=one rate=270/min
node K sink
edge W S > K.in mk=3 slots=4
";
    let b = a.replace(" > ", " -> ");
    let mut wa = parse(a).unwrap();
    let mut wb = parse(&b).unwrap();
    wa.run_ticks(secs(20.0));
    wb.run_ticks(secs(20.0));
    assert_eq!(wa.state_hash(), wb.state_hash());
}

// DETERMINISM: shuffle mode permutes processing order deterministically per
// seed. A law-abiding circuit's settled results are identical across seeds
// (tie-breaks move, outcomes do not); the same seed reproduces exactly.
#[test]
fn test_shuffle_order_insensitivity() {
    let mut base = parse(SEAL_FLUSH).unwrap();
    base.run_ticks(secs(40.0));
    let flow = base.sink_count("K", "one");
    let pills = base.sink_count("K", "zero");
    assert_eq!(pills, 1);

    for seed in [1u64, 42, 0xBEEF] {
        let mut w = parse(SEAL_FLUSH).unwrap();
        w.set_shuffle(Some(seed));
        w.run_ticks(secs(40.0));
        assert_eq!(
            w.sink_count("K", "one"),
            flow,
            "settled FLOW differs under shuffle seed {}",
            seed
        );
        assert_eq!(w.sink_count("K", "zero"), 1, "seed {}", seed);

        let mut w2 = parse(SEAL_FLUSH).unwrap();
        w2.set_shuffle(Some(seed));
        w2.run_ticks(secs(40.0));
        assert_eq!(w.state_hash(), w2.state_hash(), "same seed must reproduce");
    }
}
