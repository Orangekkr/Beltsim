# .blt language reference (v0.4)

One statement per line. `#` comments. Tokens split on whitespace; no spaces
inside key=value or comma lists. Names: whitespace-free tokens without
`.` `,` `:` `=` `#` `/`. Items, edges, displays, modules, pins are separate
namespaces; instances share the node namespace.

Order: item before use; node before its edges; display and probe after
their targets; at resolves after the whole file (convention: end of file);
def before inst; default and alias before the lines they should affect.

Determinism: expanded declaration order is entity id order is processing
order. Same-tick stimuli apply in file order. Reordering lines changes
tie-breaks (RR seeds, one-tick hop races) without being a bug; see shuffle
below for hunting circuits that secretly depend on those tie-breaks.

## item

    item <name>

Idempotent. May not be named any, undefined, or overflow.

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
pmerger: strict priority high/med/low; one-low-per-high-gap buffer leak;
tiers remappable at runtime.
container: type+count preserved, order+timing destroyed (FIFO here).
gate: test valve, not a game entity. Default open unless open_at given.

## edge

    edge <n> <from>[.<port>] > <to>[.<port>] (mk=<1..6>|rate=<r>/min) slots=<n>
         [lift] [mouth=0|1] [prefill=<i>:(full|<n>)] [rule=<rulelist>]

`>` is the arrow; the old `->` still parses. slots >= 1, about 1.2 m each.
slots[0] is the head. mk rates: 60/120/270/480/780/1200 per min. rate= must
divide 280800 per minute exactly; the parser refuses to approximate.

lift: mouth by default (extra entrance slot; capacity slots+1; 1-slot lift
seals at 2). mouth=0 = tight coupling. Mouth not prefillable. Pre-bench.

rule= required leaving a smartsplitter, forbidden elsewhere. rulelist is
comma-separated items and keywords: <item> exact; any competes with exact;
undefined matches only items with no exact filter on this splitter;
overflow is the last resort. All pre-bench.

## default (scoped)

    default <kind> key=value ...

kind: edge, source, button, container, gate. Fills MISSING keys on later
statements of that kind; explicit keys always win. mk and rate are one
exclusive group: a statement carrying either suppresses both defaults.
rule, mouth, and lift cannot be defaulted. Re-defaulting a key overrides.

Scoping is lexical: each file has its own scope (used files never leak
into the importer); a def captures the scope at its DEFINITION site, plus
default lines inside its own body. Instantiation-site defaults never reach
a module's internals.

## alias (scoped)

    alias <name> <token> [token ...]

Whole-token substitution, applied once per line (no recursion), before
anything else is parsed. Multi-token expansions splice in place:
alias fast mk=6 slots=8 makes `edge W A > B fast` legal. Scoped exactly
like defaults. Aliases can shadow anything, including keywords and node
names: that power is yours to misuse.

## ports

    source/button: out (or omit)      sink: in (or omit)
    splitter/smartsplitter: in; out0 out1 out2, contiguous from out0
    merger: in0 in1 in2; out          pmerger: high med low; out
    container/gate: in; out

One edge per port.

## display

    display <n> lamp <edgeref>
    display <n> meter <edgeref>
    display <n> register <ref_msb>,...,<ref_lsb> [one=<item>] [zero=<item>]
    display <n> counter <sink> [item=<item>]

An <edgeref> is an edge name, or a pin/port reference (inst.pin, node.port)
that resolves to the edge attached there. probe accepts the same forms.

lamp: level classification (NULL, LEVEL of a type, partial, full-mixed).
meter: occupancy, lifetime in (items entered, prefill included), total
(items delivered off the head), and lifetime average delivery rate per
second.
register: per bit, saturated one-item = 1, saturated zero-item = 0, empty
= N, else ?. All 0/1 decodes to hex+dec, otherwise flagged not settled.
one=/zero= default to items literally named one/zero.

## at (scripted stimuli)

    at <sec> rule <node>.<outN> <rulelist>
    at <sec> priority <pm> high=<edge> [med=<edge>] [low=<edge>]
    at <sec> open <gate> | close <gate>
    at <sec> press <button> [n]

Applied at that simulated second, before sources, file order within a tick.
Same grammar as the play commands. Hierarchical names work (rule
bit/CAP.out0 any); pins do not: address internals by full path in actions.

## modules

    def <name>
    node ... | edge ... | display ... | probe ...
    pin <pin> <node>[.<port>]
    inst <child> <module>
    def <inner> ... end
    default ... | alias ...
    end

    inst <name> <module>
    use <path>

def bodies allow node, edge, display, probe, pin, inst, nested def,
default, alias. No items, no at. pin aliases an internal node port;
direction implied by the port, checked on use; pins may forward a nested
instance's pin (pin i x.i). Unwired required pins surface as the internal
node's connectivity error, which names the exact node.

inst inlines the body: internal names become <inst>/<name>, recursively
(a/b/NODE). Zero runtime cost; tick-identical to hand-writing (hash-equal,
tested). Recursion is a parse error, including through shadowed names.
Nested defs are visible inside their enclosing def and below, invisible
outside; they capture the scope at their definition point in the body.

use imports item, def, default, and alias statements from another file
(path relative to the importing file, idempotent, cycles ignored), but the
imported defaults and aliases stay local to that file. Anything else at a
used file's top level is an error.

## shuffle (order-race hunting)

    beltsim run <file> --shuffle <seed>
    world.set_shuffle(Some(seed))

Permutes junction and belt processing ORDER within each phase,
deterministically per seed. Semantics are unchanged except declaration-
order tie-breaks. A circuit whose settled results differ across seeds has
a hidden order race; per the catalog philosophy such a design should be
reworked until shuffle cannot touch it. Same seed always reproduces the
same run.

## seconds

Decimal seconds, converted at 4680 ticks per second, rounded. Belt periods
are exact by construction.

## errors you will meet

rate does not divide the grid; smartsplitter edge missing rule=; out ports
not contiguous from out0; mouth on non-lift; prefill exceeds slots; port
already used; unknown or duplicate names; recursion; used-file top-level
violations; rule/mouth/lift in a default; connectivity arity. Errors carry
file:line and the instance path when inside one.
