// Benchmark circuit synthesis. Two unit flavors:
//
// CHURN unit: a circulating half-full ring through a merger and splitter,
// with a source feeding in and a sink bleeding off at the splitter. Every
// belt in it is partially full and moving on every cadence tick, so this is
// close to the worst case for the naive oracle: maximum slot-scan work per
// sim second.
//
// SETTLED unit: a saturated prefilled belt parked against a closed gate.
// After tick 0 nothing in it ever changes. This measures the cost of held
// levels, which is where a real belt CPU spends most of its life
// (settled-state logic over transition logic).
//
// The interesting number is the real-time multiple: simulated seconds per
// wall second. Target from the architecture report: 1000x on CPU-scale
// circuits.

pub fn churn_netlist(units: usize) -> String {
    let mut s = String::with_capacity(units * 300 + 64);
    s.push_str("item one\n");
    for i in 0..units {
        s.push_str(&format!("node S{i} source item=one rate=240/min\n"));
        s.push_str(&format!("node M{i} merger\n"));
        s.push_str(&format!("node SP{i} splitter\n"));
        s.push_str(&format!("node K{i} sink\n"));
        s.push_str(&format!("edge Ea{i} S{i} -> M{i}.in0 mk=4 slots=6\n"));
        s.push_str(&format!(
            "edge Eb{i} M{i}.out -> SP{i}.in mk=4 slots=15 prefill=one:7\n"
        ));
        s.push_str(&format!(
            "edge Ec{i} SP{i}.out0 -> M{i}.in1 mk=4 slots=15 prefill=one:7\n"
        ));
        s.push_str(&format!("edge Ed{i} SP{i}.out1 -> K{i}.in mk=4 slots=6\n"));
    }
    s
}

pub fn settled_netlist(units: usize) -> String {
    let mut s = String::with_capacity(units * 220 + 64);
    s.push_str("item one\n");
    for i in 0..units {
        s.push_str(&format!("node S{i} source item=one rate=60/min\n"));
        s.push_str(&format!("node G{i} gate initial=closed\n"));
        s.push_str(&format!("node K{i} sink\n"));
        s.push_str(&format!(
            "edge Ea{i} S{i} -> G{i}.in mk=1 slots=12 prefill=one:full\n"
        ));
        s.push_str(&format!("edge Eb{i} G{i}.out -> K{i}.in mk=1 slots=4\n"));
    }
    s
}
