# Graph GUI review and implementation handoff

Date: 2026-09-13. Reviewed revision: `d25c90f`.

## Main finding

The user's concern is supported by the implementation. Connections can be difficult to see, deliberately absent from the stored model, or hidden by filters without an explanation. The interface gives little help distinguishing these cases. Clicking a node opens technical facts, but clicking an edge has no application behavior, and neither selection provides a direct route to an explanation.

The first implementation should make existing relationships legible and selectable, connect selection to grounded explanations, and make the graph's scope explicit. Keep Sigma/Graphology initially: the most immediate problems are in application styling, state, and data contracts. A renderer replacement would not resolve the missing context or aggregation semantics.

This report proposes changes for another agent; no application code was changed.

## Evidence and limits of this review

Reviewed the React graph, layout, filters, selection, details, search, and chat code; the GUI REST/DTO/chat implementation; aggregate graph construction and its existing tests; and the separate cell-level graph implementation. File links below are relative to this report. Line numbers refer to the reviewed revision.

Source inspection confirms the behaviors described as confirmed below. Visual severity, browser event behavior, and timing races still need interactive reproduction. No running GUI or indexed user workbook was inspected, and no browser screenshot or performance benchmark was produced. Frontend dependencies are absent from `frontend/node_modules` in this checkout.

Two small calculations were run with Node: relative luminance contrast for the configured colors, and available canvas width from the CSS grid. These support the visibility findings but are not a rendered accessibility audit.

Attempted `cargo test --offline -p eg-graph --test build`; it stopped before compilation because the workspace could not resolve the uncached `async-openai` dependency required by `eg-gui`. Existing tests cited here were read, not successfully executed during this review.

## Findings, ordered for implementation

### 1. All edges use the dimmed color, contradicting the legend — high priority, confirmed

[GraphView.tsx](../frontend/src/graph/GraphView.tsx), lines 88–95, assigns `color: DIM_EDGE` to every edge. Its reducer never restores a kind-specific color. [theme.ts](../frontend/src/graph/theme.ts) defines distinct `EDGE_COLORS`, and [Legend.tsx](../frontend/src/components/Legend.tsx) displays those colors, but GraphView does not import that palette.

The actual edge color is `#20242c` on a `#0b0d10` background from [styles.css](../frontend/src/styles.css). Calculated contrast is approximately **1.25:1**. For comparison, the configured dependency blue would be about 7.20:1 and cross-sheet orange about 8.01:1 against that background. Thin, overlapping edges will be particularly hard to follow; the practical severity needs browser verification.

Recommendation: use kind-specific colors in the ordinary view, a readable subdued treatment for structural edges, and the dim color only when deliberately deemphasizing context. Give selected and hovered relationships an unmistakable treatment. Keep the canvas and legend driven by the same style definitions, including a fallback for unknown kinds.

Acceptance: with all filters enabled, a small mixed graph visibly distinguishes containment, dependencies, cross-sheet references, and name references. The same edge retains its meaning across ordinary, selected, and answer views.

### 2. Direction and overlapping relationships are not readable — high priority

The graph is directed, but [GraphView.tsx](../frontend/src/graph/GraphView.tsx) assigns no edge rendering type and configures no arrow renderer. Sigma's documented default edge renderer draws straight rectangles; it does not infer an arrow from Graphology's directed graph type. Edge labels are explicitly disabled, and edge attributes do not include explanatory labels. See the official [Sigma renderer documentation](https://www.sigmajs.org/docs/advanced/renderers/).

Straight edges joining the same two positions also provide no separation for reciprocal or parallel relationships. The application supports a multigraph but supplies no curve assignment or relationship chooser. Overlap is a rendering risk established by the design; its frequency in the user's workbook is unmeasured.

Recommendation: show arrowheads for dependencies, at least in the focused neighborhood, and expose source, relation, and target on hover and selection. Separate reciprocal/parallel edges with curves or offsets; where aggregation remains necessary, show a multiplicity badge and a selectable list of the underlying edges. Labels should appear on interaction or useful zoom levels rather than all at once.

Use the engine's direction consistently: **source reads target** for `DEPENDS_ON` and `CROSS_SHEET_REF`; source references the external workbook or defined name for those edge types. A dependency arrow points from the consuming region to the region it reads. Explain that convention in the legend. [node.rs](../crates/eg-graph/src/node.rs), lines 219–239, defines these semantics.

### 3. Edge selection and selection-aware explanations are missing — high priority, confirmed

[GraphView.tsx](../frontend/src/graph/GraphView.tsx), lines 203–204, registers only `clickNode` and `clickStage`. There is no edge click/hover handler, edge selection state, or edge inspector. [DetailsPanel.tsx](../frontend/src/components/DetailsPanel.tsx) accepts node details and node navigation only; it offers no explanation action. Neighbor rows navigate to the other node, so they cannot inspect the relationship itself.

[App.tsx](../frontend/src/App.tsx) sends chat text with the default session ID. [api.ts](../frontend/src/api.ts), `postChat`, and server [api.rs](../crates/eg-gui/src/api.rs), `ChatBody` at line 304, transmit neither the open workbook nor the selected entity. [chat.rs](../crates/eg-gui/src/chat.rs), `run_turn`, retrieves using remembered session scope. Thus selecting a node and typing “What is this?” does not identify that node to the backend, and an existing conversation can retain scope from another workbook.

Recommendation: implement the interaction and structured context contract in the next section. Merely inserting a node label into the text box is insufficient: labels such as “Total” recur across sheets and workbooks, and text search need not retrieve the selected object.

Implementation note: edge events must be enabled as well as handled. The lockfile resolves Sigma **3.0.3**. Consult the version's actual types before choosing the flag: the official [settings reference](https://www.sigmajs.org/docs/typedoc/sigma/src/settings/interfaces/Settings/) exposes `enableEdgeEvents`, while the prose [events guide](https://www.sigmajs.org/docs/advanced/events/) refers to separate flags. Do not copy a v4 initialization example into this v3 application. Check picking behavior and performance with the chosen edge program.

### 4. The GUI presents an aggregate graph without explaining what it omits — high priority, confirmed

The GUI endpoint serializes **every stored node and edge**: [dto.rs](../crates/eg-gui/src/dto.rs), `graph_dto` at line 122, iterates `node_indices()` and `edge_references()` without a display limit. No evidence was found of the REST endpoint arbitrarily dropping stored connections. However, the stored graph itself is deliberately lossy.

| What a user may expect | What the engine actually stores | Required GUI explanation/action |
| --- | --- | --- |
| A link between each formula cell and its inputs | References are lifted to regions, with identical relationships merged and counted | Label the view “Region overview”; explain edge weight and provide cell tracing |
| Links between formulas inside one table/region | References within the source region are counted and omitted from the graph | State that internal references are summarized; offer “Trace cells in this region” |
| Dependency links on a column or formula-group node | Lifting starts at its owning region, or a sheet fallback if no region is found | Offer “Show region dependencies”; do not fabricate column/group dependency edges |
| Every broken or empty-target reference as a visible link | Missing-sheet and unpopulated-target cases are recorded in the build report without a normal target edge | Surface coverage counts and bounded examples |
| An external link continuing into another indexed workbook | The aggregate graph terminates at an external-workbook token node | Mark it as an unresolved external target; do not imply automatic corpus linking |
| Formula groups always available to expand in the canvas | Storage can omit the layer above 20,000 groups; the GUI has no expansion action | Replace the passive “formula groups on demand” message with an implemented action or accurate limitation |

Evidence: [build.rs](../crates/eg-graph/src/build.rs), module documentation, `drop_formula_groups`, `lift_dependencies`, `lift_reference`, and `external_node`; [store.rs](../crates/eg-graph/src/store.rs), `MAX_STORED_FORMULA_GROUPS`; [report.rs](../crates/eg-graph/src/report.rs), `BuildReport`. The report is already persisted on `StoredGraph`, but is not exposed by `GraphDto`.

Existing [build tests](../crates/eg-graph/tests/build.rs) make the distinction concrete: `identical_references_merge_into_one_weighted_edge` expects three cross-sheet references to become one edge of weight three; `a_reference_within_the_same_region_makes_no_edge` expects two internal references and zero dependency edges.

Expose a coverage summary derived from the actual stored report: scanned references, references producing edges, within-region references, external references, missing-sheet references, and unpopulated targets. Keep counters distinct: one reference can intersect multiple regions, so summed edge weights need not equal the number of formula reference expressions. Cross-sheet references are a subset of lifted references, not another disjoint outcome to add to the total. Counts concern what the scanner recognized; they are not proof of complete runtime formula evaluation.

For deeper inspection, reuse [eg-eval/src/graph.rs](../crates/eg-eval/src/graph.rs), which already provides bounded cell/range dependency graphs, including external and missing-sheet terminal nodes. The current GUI routes do not expose it. Add an explicit trace action and a separate view with depth, node limits, and cap reporting. Preserve the distinction between numeric aggregate node IDs and citation-based cell graph IDs.

### 5. Sheet filtering hides the relationships most useful for tracing — high priority, confirmed predicate

The node reducer in [GraphView.tsx](../frontend/src/graph/GraphView.tsx), lines 154–155, hides every non-workbook node whose `sheet` differs from the selected sheet. This includes cross-sheet dependency endpoints and sheetless external-workbook and workbook-scoped name nodes. The latter have no sheet by definition; see [node.rs](../crates/eg-graph/src/node.rs), `Node::sheet`.

Consequently, selecting one sheet removes other-sheet context from the visible map. The edge reducer explicitly checks endpoint *kinds* but does not share the sheet visibility predicate. Even if Sigma suppresses edges attached to hidden nodes, application counts and interaction logic need the same definition of visibility.

Recommendation: provide clearly named “This sheet only” and “This sheet + connected context” scopes. In context mode, retain one-hop boundary targets on other sheets, relevant global names, and external targets, with sheet/workbook badges. In strict mode, show how many incident relationships are hidden and offer “Reveal connected targets.” Compute visibility once for rendering, hit lists, counts, and selection handling.

The topbar and legend currently show whole-graph totals, not visible totals. Show “visible / loaded” counts with active filter chips and “Reset filters.” A details panel should distinguish an existing relationship whose target is hidden from a relationship absent from the aggregate graph.

### 6. Answer highlighting is not tied to the actual supporting edges — high priority, confirmed

[App.tsx](../frontend/src/App.tsx), `runAsk`, keeps roles by node ID and a set of edge-kind strings. It discards each retrieved node's `via` relationship when building the highlight. [GraphView.tsx](../frontend/src/graph/GraphView.tsx), `edgeReducer`, then thickens **every edge of a matching kind across the whole graph**, regardless of whether either endpoint supports the answer. These thicker edges still have the dim color.

Bare search highlights supply an empty edge-kind set, causing all edges to be thinned to 0.4 whenever there are highlighted nodes. Selecting a different node does not reveal its incident edges. Clicking the stage closes details but leaves the answer/search highlight in place; there is no dedicated clear-highlight control.

Recommendation: carry explicit supporting edge identities in retrieval output and highlight those edges and their endpoints. The existing `via`, `edge_kind`, and role data can help reconstruct an interim traversal relationship, but direction must be resolved against the actual graph, and one traversal parent is not a complete list of all answer-supporting connections. Distinguish “retrieval path” from “all edges among these nodes.” Add “Clear answer highlight,” and make node selection temporarily emphasize its actual neighborhood.

### 7. Selection and camera state can become inconsistent — medium priority, source-derived risks

[App.tsx](../frontend/src/App.tsx), `select` at line 182, uses `getNodeDetail(...).then(setDetail)` without cancellation or a request generation check. Rapid A→B clicks can allow a late A response to replace B's details. A response can also arrive after closing the panel or changing workbooks; selection is derived solely from `detail.node.id`, even though numeric IDs are local to a graph. Workbook and query loads have similar unguarded completion paths.

[GraphView.tsx](../frontend/src/graph/GraphView.tsx), focus effect near line 234, records the focus nonce before verifying that Sigma exists. Sigma is created in a 30 ms timeout. A focus request received before readiness can be consumed without being applied; assigning `sigmaRef.current` does not rerun that effect. The parent ignores `onReady`, and pending navigation is applied when graph data arrives rather than when rendering is ready. Confirm this with delayed initialization and navigation to a newly opened workbook.

Recommendations: represent selection independently of fetched details, scoped by workbook/hash; use cancellation or generation checks for detail, graph, and query requests; clear or reconcile selection on graph replacement. Queue camera requests until readiness and consume them only after application. If a selected target is filtered out, offer to reveal it instead of silently focusing an invisible node. Keep a graph click's camera position stable by default and provide explicit “Focus” and “Fit neighborhood” actions.

### 8. Layout and panel behavior make tracing harder — medium priority

The grid in [styles.css](../frontend/src/styles.css), line 68, reserves 300 px for each side dock; details reserve another 320 px at line 553. With details open, nominal graph width is only 360 px at a 1280 px viewport, 520 px at 1440, and 1000 px at 1920, before other layout constraints. The legend overlays that remaining space. Opening details therefore changes the map's available area substantially just as the user begins inspecting a node. There are no responsive media queries in this stylesheet.

Recommendations: collapse/rescale side docks; combine inspector and explanation in a tabbed or resizable right dock; preserve a useful canvas width at common laptop sizes; keep the selected neighborhood visible when a panel opens. Add visible zoom, fit graph, fit selection/neighborhood, back, and reset controls. Provide keyboard-accessible node and relationship lists with the same actions as canvas selection; tiny GPU edge targets should not be the only way to inspect a connection.

ForceAtlas2 runs synchronously inside `useMemo`, before the effect sets “laying out.” The subsequent timeout delays renderer creation, not layout computation. The source comment claiming that status paints before layout is inaccurate. Runtime cost has not been measured. Move expensive layout off the main thread, cancel obsolete work when changing workbooks, and report readiness accurately. Benchmark realistic large graphs before promising a loading duration.

The force layout also receives raw reference counts as edge weights. Test a capped/logarithmic layout weight separately from the original count used in explanations; otherwise very heavily referenced regions may dominate spatial arrangement. Preserve positions while filtering. Later, compare a sheet-grouped overview and a layered dependency neighborhood with the current all-node force layout. These are design experiments, not reasons to delay the earlier fixes.

### 9. The inspector mislabels structural relationships — medium priority, confirmed

[DetailsPanel.tsx](../frontend/src/components/DetailsPanel.tsx) calls all incoming edges “read by,” including `CONTAINS` and `HEADER_OF`. It calls all outgoing edges of regions and columns “reads,” including structural child links. The raw edge-kind text underneath does not repair the misleading section heading.

Group by relationship semantics: “Contains / Contained in,” “Heads formula groups / Headed by,” “Reads / Read by,” and “Uses defined names.” Show direction, sheet-qualified endpoint labels, and the meaning of weight. Keep advanced raw payload fields in an expandable section below a short human-readable description.

## Proposed node/edge explanation workflow

1. **Select:** clicking a node or edge immediately marks it selected and opens one inspector. An edge highlights both endpoints. Hover gives a compact preview with qualified labels and relationship direction. Selection never automatically sends a chat request.
2. **Understand immediately:** show a deterministic description from stored facts. Example: “This region on Sales reads Rates!A2:A4. This cross-sheet edge aggregates 3 references.” For a structural edge: “Sales contains this region.” Describe weight as a reference count, not confidence, unique cells, or business importance.
3. **Explain:** a prominent “Explain this node” or “Explain this connection” button sends an explicit, structured selection snapshot to the explanation/chat pipeline. The input also supports “Ask about this…” with a visible context chip. The user can change the question or remove the context.
4. **Follow evidence:** the response cites workbook/sheet/ranges and offers selectable source and target links. “Show inputs,” “Show dependents,” “Show region dependencies,” and “Trace cells” preserve context and return navigation. A formula example is labelled as an example, with the underlying aggregate count kept separate.
5. **Continue or recover:** follow-ups retain the explicit entity context visibly. Closing/changing selection has a clear effect on the next message; previous turns remain attached to the entity they originally described. Missing/stale objects, unavailable source workbooks, failures, and capped traces receive inline status with retry or navigation actions.

The immediate description must work without an LLM. Existing chat is offline retrieval by default, with optional composition controlled by privacy settings; preserve that behavior. An optional composed answer should explain the selected facts and bounded evidence, and identify uncertainty about business purpose. Reuse existing redaction and LLM-privacy handling when retrieving examples or persisting chat.

### Suggested implementation contract

Introduce a discriminated selection object, for example:

```ts
type GraphSelection =
  | { workbook: string; entity: "node"; nodeId: number }
  | { workbook: string; entity: "edge"; edgeId: string };
```

Add a graph-scoped edge ID to server `EdgeDto`, frontend `EdgeDto`, and neighbor details. Today Graphology creates edge keys internally and the DTO has only source/target/kind/weight. Prefer a server-issued ID stable for the served graph version, and use `addDirectedEdgeWithKey`. Do not identify an edge by endpoints alone: the model is a multigraph. If IDs can change under reindexing or pruning, invalidate old selections explicitly and consider an additional graph generation token beyond the content hash.

Extend the chat/explanation request with optional structured context and an action such as `explain`. Validate workbook and entity on the server. Resolve the selected entity directly, gather its bounded neighborhood, and render its evidence before optional composition. Explicit request context must take precedence over session scope and query condensation; free-text search must not substitute another similarly named entity. Keep message-only callers working, including MCP chat callers.

An edge's aggregate DTO has no formula provenance. A complete “why does this edge exist?” answer needs bounded tracing against the source workbook, constrained to the source region and matching target/kind. Reuse trace/reference resolution logic and distinguish a sampled explanation from exhaustive accounting. If the workbook is missing or changed, the stored edge can still be described, but current formula evidence must not be invented or attributed to the old graph.

Return structured entity/citation links with explanations, not just text joined with separators. If using the shared chat log, carry the context through response DTOs, persistence, and broadcasts so tabs and agents can identify what was explained. Keep identifiers scoped when highlighting a response after the user switches workbooks.

## Implementation sequence and verification

### First deliverable: legible, selectable, explainable existing graph

Fix edge colors and direction; introduce stable edge identity, edge picking, unified node/edge selection, and semantic details headings. Add deterministic explanations and the direct “Explain” action with structured server context. Add visible/loaded counts, clear-highlight/reset-filter controls, and precise incident/supporting-edge highlighting. Address stale detail responses as part of introducing selection state.

Primary files: [GraphView.tsx](../frontend/src/graph/GraphView.tsx), [theme.ts](../frontend/src/graph/theme.ts), [App.tsx](../frontend/src/App.tsx), [DetailsPanel.tsx](../frontend/src/components/DetailsPanel.tsx), [ChatPanel.tsx](../frontend/src/components/ChatPanel.tsx), frontend [types.ts](../frontend/src/types.ts)/[api.ts](../frontend/src/api.ts), and server [dto.rs](../crates/eg-gui/src/dto.rs)/[api.rs](../crates/eg-gui/src/api.rs)/[chat.rs](../crates/eg-gui/src/chat.rs).

### Second deliverable: honest scope and useful traversal

Expose build coverage and aggregate semantics; implement sheet-plus-context scope; add bounded cell tracing using the existing evaluation engine; reconcile camera requests with renderer readiness. Provide disclosure for omitted formula groups and unavailable trace evidence.

### Third deliverable: space and performance

Make panels collapsible/resizable, add camera/history controls and accessible lists, separate background layout work, and evaluate grouped overview versus focused dependency layouts. Preserve existing filter-without-relayout behavior. Optimize node detail fetching: the current handler reconstructs the entire graph DTO for each selected node, which is unnecessary work to benchmark on large workbooks.

### Required acceptance scenarios

| Scenario | Expected outcome |
| --- | --- |
| Small graph containing all six relationship kinds | Canvas styles match the legend; selected source/relation/target and arrow direction are unambiguous |
| Select a node, then an edge, then Explain | Each action describes the exact selected entity; the edge's inspector and explanation include both endpoints |
| Duplicate labels across sheets and workbooks; session previously discussed another workbook | Explicit selection wins; no answer is silently retrieved for the other “Total” |
| Reciprocal/parallel edges between two nodes | Each relationship is distinguishable and selectable, including through the relationship list |
| Sheet A reads sheet B and uses a global name/external reference | Strict mode reports hidden relationships; context mode exposes relevant targets with scope badges |
| Hide a node kind, run search/ask, clear highlight, reset filters | Counts match visibility; unrelated same-kind edges never masquerade as answer evidence; restoration is predictable |
| Select a column/group with no outgoing dependency edge | Inspector explains aggregation and offers parent-region dependencies or bounded cell tracing |
| Internal references, missing-sheet references, and empty targets | Coverage describes why ordinary edges are absent; bounded trace exposes available details without claiming completeness |
| Delay A's detail response; click B, close details, or switch workbook | Late A results cannot reopen or replace the current selection |
| Navigate to a workbook before renderer initialization completes | Camera focus occurs after readiness and does not disappear into a hidden node or blank space |
| 1280×800 and 1440×900 windows, details/chat open | Selected neighborhood remains usable, docks can collapse, and controls do not cover the essential graph |
| Large graph and rapid workbook switches | UI remains responsive, obsolete layout work is discarded, and loading status reflects actual work |
| LLM unavailable/off; values redacted; source workbook unavailable | Deterministic descriptions remain useful; settings are respected; unavailable evidence is stated accurately |

Use focused frontend interaction tests for picking/selection/filter/highlight behavior and browser checks for actual arrow visibility, edge hit targets, panel resizing, and camera timing. Add backend tests for entity-context resolution, stable edge identity, stale IDs, and explanation grounding. Keep existing aggregate graph tests as semantic guards. Benchmark layout and picking at representative node/edge counts, recording counts, viewport, browser, and timings; do not assume the current source comment's “well under a second” claim.

## Response (branch `graph-gui-legibility-fixes`)

Read every cited line before touching anything and confirmed all of it against the source rather than trusting the report: the all-dim edges, the missing edge handlers, the free-text-only chat body, the `edgeKinds`-not-`edgeIds` highlight, the unguarded `getNodeDetail(...).then(setDetail)`, the focus-nonce-before-readiness race, the "read by" mislabeling, and the DTO shapes (`EdgeDto` had no `id`, `GraphDto` had no coverage) were all exactly as described. No disagreement with the diagnosis. This response is about what got fixed now versus deferred, and one thing the review couldn't have caught from static reading.

**Fixed — findings 1, 3, 6, 7, 9, and the honest-scope half of 4 and 5:**

- Edge colors now match the legend (`EDGE_COLORS` bumped from ~1.25:1 to ~3:1/~7-8:1 contrast); `GraphView` imports the shared palette instead of hardcoding `DIM_EDGE` for everything.
- Edge selection is real: `EdgeDto` carries a stable `id` (the petgraph `EdgeIndex`, the same stability guarantee `NodeDto.id` already has — no separate version-token layer introduced for edges alone), `enableEdgeEvents: true`, `clickEdge`/`enterEdge`/`leaveEdge` wired, a discriminated `Selection` type replaces the old bare `selected: number | null`, and the details panel renders a full edge card (source/target/weight, a deterministic sentence, "Explain this connection") with no backend call needed for the immediate description.
- Chat now takes an optional `{workbook, node}` context (`chat::EntityContext`). When present it skips free-text search and LLM condensation entirely and expands directly from the selected node (`api::ask_engine_for_node`), so an explicit selection always outranks stale session scope or a same-named node elsewhere — covered by a new test, `an_explicit_selection_overrides_the_free_text_query`. Verified end-to-end in a browser: selecting an edge, clicking "Explain this connection", and sending grounds the answer on the selected region, not a text search.
- Answer highlighting now carries `edgeIds: Set<string>` reconstructed from each retrieved node's `via`/`edge_kind` matched against the graph's real edges in either direction, not "every edge of a kind that appears anywhere in the roles." Verified in a browser: an `ask` now lights up exactly the one supporting edge instead of thickening every same-kind edge in the graph.
- Selection races: a generation counter guards `getNodeDetail`, so a stale response for an abandoned selection is dropped rather than reopening the panel; camera focus requests are queued and applied once Sigma is actually ready instead of being marked consumed and dropped.
- Details panel groups by what an edge kind actually means (Contains/Contained in, Heads formula groups/Headed by, Reads/Read by, Uses defined names/Named by) instead of raw in/out direction.
- `GraphDto` now carries a `coverage` summary straight from the stored `BuildReport` (references scanned/lifted/within-region/cross-sheet/external/dangling/unpopulated, unknown sheet names); the legend has a "region overview: what this omits" disclosure panel, and the topbar's formula-groups message states the limitation plainly instead of implying an action that doesn't exist.
- Sheet filtering gained "this sheet only" vs "this sheet + connected context" (one dependency hop off-sheet, workbook-scoped names, external targets kept visible); "reset filters" and "clear highlight" controls added; direction convention stated in the legend (arrows point from what depends to what it depends on).

**One thing static reading couldn't have caught:** a curved-edge program (`@sigma/edge-curve`, the natural way to separate reciprocal/parallel edges per finding 2) was tried first. It registered and compiled without error but rendered *no edges at all* on this exact stack (sigma 3.0.3 + `@sigma/edge-curve` 3.1.0) — a real, reproducible integration break, not a hypothetical risk. Reverted to Sigma's own `EdgeArrowProgram`/`EdgeLineProgram`, confirmed edges and arrowheads render correctly in a live browser session against the demo workbook. Parallel/reciprocal pairs still overlap into one line (both arrowheads visible, which is at least truthful about mutuality); separating them is deferred to a follow-up that budgets time to root-cause the integration rather than ship a half-verified dependency.

**Deferred, on purpose, not silently dropped:**

- Bounded cell-level tracing via `eg-eval::graph::subgraph` and a "rebuild formula groups on demand" action (the last two rows of finding 4's table) are new endpoints plus new UI, not a fix to what's there — exactly the "second deliverable" scale this review's own sequencing proposes. The coverage disclosure above tells the user honestly that this is missing, which is the load-bearing part of finding 4; the trace action itself is follow-up work.
- Resizable/collapsible docks and moving ForceAtlas2 off the main thread (finding 8) are un-benchmarked by this review's own admission ("Runtime cost has not been measured"), and the review itself calls the layout-comparison half "design experiments, not reasons to delay the earlier fixes." Agreed with that framing: fixed the inaccurate status-comment wording and added a `fit` camera control and viewport breakpoints that keep the canvas usable at 1280/900px with panels open, but did not build the interactive collapse machinery or chase a performance number nobody has measured yet.
- The full `GraphSelection`/version-token wire contract sketched under "Suggested implementation contract" wasn't built as its own layer — the simpler version (edge id = stable `EdgeIndex`, explicit chat context = `{workbook, node}`) covers the acceptance scenarios that matter (duplicate labels, stale session scope, edge inspection) without adding a second identity scheme alongside the one nodes already use.

No finding in the original review was rejected outright; everything above is either fixed, or deferred with a stated reason.
