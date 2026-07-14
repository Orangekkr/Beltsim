# .blt language reference (v0.3)

One statement per line. `#` comments. Tokens split on whitespace; no spaces
inside key=value or comma lists. Names: any whitespace-free token without
`.` `,` `:` `=` `#` `/` (slash is reserved for instance paths). Items,
nodes, edges, displays, modules, instances, pins are separate namespaces,
except instances share the node namespace for collision purposes.

Order: item before use; node before its edges; display and probe after
their targets; at resolves after the whole file, convention is end of file;
def before inst.

Determinism: expanded declaration order is entity id order is processing
order. Same-tick stimuli apply in file order. Reordering lines changes
tie-breaks (RR seeds, one-tick hop races) without being a bug.

## item

    item <name>

re-declaring an existing name is a no-op (libraries re-declare
freely). May not be named any, undefined, or overflow.

## node

    node <n> source item=<i> [rate=<r>/min] [limit=<n>] [start=<s>] [stop=<s>]
    node <n> button item=<i>
    node <n> sink
    node <n> splitter
    node <n> smartsplitter
    node <n> merger
    node <n> pmerger
    node <n> container [cap=<n>] [prefill=<i>:<n>]
    node <n> gate [initial=open|closed] [open_at=<s>] [close_at=<s>]

source: no rate = saturate the output edge. Blocked source holds one
pending item, never backlogs; outside its window it never emits.
button: emits only on press; queued counts are exact.
splitter: per-output 1-item buffers, RR, skips occupied.
smartsplitter: rules on out edges; unroutable head = poison jam, all types.
merger: central 1-item buffer, RR; blocked out stalls all ins.
pmerger: strict priority high/med/low; central buffer leaks one low per
high gap; tiers remappable at runtime.
container: type+count preserved, order+timing destroyed (FIFO here).
gate: test valve, not a game entity. Default open unless open_at given.

## edge

    edge <n> <from>[.<port>] -> <to>[.<port>] (mk=<1..6>|rate=<r>/min) slots=<n>
         [lift] [mouth=0|1] [prefill=<i>:(full|<n>)] [rule=<rulelist>]

slots >= 1, about 1.2 m each. slots[0] is the head. mk rates:
60/120/270/480/780/1200 per min. rate= must divide 280800 per minute
exactly; the parser refuses to approximate.

lift: mouth by default (extra 1-item entrance buffer; capacity slots+1;
1-slot lift seals at 2). mouth=0 = tight splitter-port coupling. Mouth not
prefillable. Pre-bench.

rule= required leaving a smartsplitter, forbidden elsewhere. rulelist is
comma-separated items and keywords:
  <item>     exact filter
  any        matches everything, competes with exact (RR)
  undefined  matches only items with no exact filter on this splitter
  overflow   last resort when the eligible set is empty or all occupied
All pre-bench.

## ports

    source/button: out (or omit)      sink: in (or omit)
    splitter/smartsplitter: in; out0 out1 out2, contiguous from out0
    merger: in0 in1 in2; out          pmerger: high med low; out
    container/gate: in; out

One edge per port.

## display

    display <n> lamp <edge>
    display <n> register <e_msb>,...,<e_lsb> [one=<item>] [zero=<item>]
    display <n> counter <sink> [item=<item>]

register decode per bit: saturated one-item = 1, saturated zero-item = 0,
empty = N, else ?. All 0/1 decodes to hex+dec, otherwise flagged not
settled. one=/zero= default to items literally named one/zero.

## at (scripted stimuli)

    at <sec> rule <node>.<outN> <rulelist>
    at <sec> priority <pm> high=<edge> [med=<edge>] [low=<edge>]
    at <sec> open <gate>
    at <sec> close <gate>
    at <sec> press <button> [n]

Applied at that simulated second, before sources, file order within a tick.
Same grammar as the play commands. priority must assign every attached
input. Hierarchical names work: rule bit/CAP.out0 any. Pins do not: address
internals by full path in actions.

## modules

    def <name>
    node ...
    edge ...
    display ...
    probe ...
    pin <pin> <node>[.<port>]
    inst <child> <module>
    end

    inst <name> <module>

    use <path>

def bodies allow only the six statements above (no items, no at). pin
aliases an internal node port; direction is implied by the port and checked
on use. Pins may forward a nested instance's pin (pin i x.i).

inst inlines the body: internal names become <inst>/<name>, recursively
(a/b/NODE). A pin is an alias, not a boundary node, so leaving a required
pin unwired reports as the internal node's connectivity error (merger g/M
needs at least one input), which names the exact node. Zero runtime cost; tick-identical to hand-writing. Recursion is
a parse error. External wiring targets pins (edge W S -> g.a ...) or full
internal paths (probe g/O, at 5 rule g/SS.out0 ...). Displays and probes
inside a def expand per instance (g/R).

use imports item and def statements from another file, path relative to the
importing file, idempotent, cycles ignored. Anything else at a used file's
top level is an error.

## seconds

Decimal seconds, converted at 4680 ticks per second, rounded. Belt periods
are exact by construction.

## errors you will meet

rate does not divide the grid; smartsplitter edge missing rule=; out ports
not contiguous from out0; mouth on non-lift; prefill exceeds slots; port
already used; unknown or duplicate names; recursion; used-file top-level
violations; connectivity arity (merger with no output, and so on). Errors
carry file:line and the instance path when inside one.
