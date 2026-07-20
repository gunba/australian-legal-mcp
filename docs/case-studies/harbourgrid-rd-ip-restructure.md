# Case study: HarbourGrid's R&D program and offshore IP restructure

## Purpose

This fictional case tests whether an agent can use Australian Legal MCP to
produce a source-grounded, reviewable tax analysis rather than a generic answer.
It is deliberately incomplete in places: a good answer must identify factual
and evidentiary gaps instead of inventing facts.

Assume advice is requested on 15 July 2025 for the year ended 30 June 2025.
Amounts are exclusive of GST unless stated otherwise. All entities use a 30 June
year end.

## Group and activities

HarbourGrid Pty Ltd (`HG Australia`) is incorporated in Australia. Its directors
and senior executives work in Perth. It develops an AI-assisted electricity-grid
optimisation platform and a modular battery-control unit. Its aggregated turnover
for 2024–25 is expected to be AUD 17 million and it has no exempt income.

HarbourGrid Singapore Pte Ltd (`HG Singapore`) is wholly owned by HG Australia.
It has two employees in Singapore: a sales manager and a software engineer.
Strategic product, funding, pricing, and IP decisions for HG Singapore are made
at HG Australia's Perth board meetings. The Singapore directors have generally
signed the Perth team's recommendations without substantive changes.

HG Australia self-assessed the following activities as registered R&D activities
for 2024–25. Registration is assumed to have been lodged on time, but no overseas
finding was sought:

1. iterative training and testing of grid-failure prediction models;
2. design and destructive testing of prototype battery-control units;
3. customer-specific configuration and routine bug fixes after commercial
   release; and
4. Singapore-based latency experiments conducted by HG Singapore's engineer.

## 2024–25 expenditure and receipts

HG Australia's general ledger records:

| Item | AUD | Additional facts |
|---|---:|---|
| Australian engineer salaries and on-costs | 5,800,000 | Engineers kept project timesheets, but descriptions vary in quality. |
| Cloud compute and data licences | 1,200,000 | Engineering estimates 70% experimental model training and 30% production/customer analytics; invoices are not tagged by use. |
| Prototype materials and fabrication | 1,600,000 | Some prototypes were destroyed. Units with production cost of 500,000 were later sold to customers for 1,200,000. |
| Pilot hardware decline in value | 900,000 | Hardware was used in experiments for nine months, then redeployed in the commercial service. |
| HG Singapore experimentation charge | 2,000,000 | Cost-plus invoice; no overseas finding. The underlying Singapore payroll and cloud cost was 850,000. |
| Founder-controlled contractor | 1,100,000 | The contractor is an Australian company owned 60% by HG Australia's founder. Only 400,000 was paid by 30 June 2025; the balance was paid in September 2025. Work descriptions are broad. |
| Patent, freedom-to-operate, and trade-mark legal fees | 600,000 | The patent work relates to the platform and battery controller. |
| Interest on project borrowing | 400,000 | Borrowing funded the mixed experimental/commercial program. |

HG Australia received:

- a 3,000,000 non-refundable Commonwealth clean-energy grant expressly tied to
  the registered project and paid in two instalments during 2024–25; and
- 1,200,000 proceeds from the prototype units described above.

The CFO proposes claiming all 13,600,000 of expenditure as an R&D notional
deduction. The company applies a 25% corporate tax rate and expects a tax loss
before any R&D tax offset.

## 30 June IP restructure

On 30 June 2025 HG Australia assigned all existing platform source code,
algorithms, patents, know-how, and related rights to HG Singapore for 2,000,000.
The price was based on accumulated legal and development cost. A valuation
commissioned after year end estimates a 25,000,000 arm's-length value at the
transfer date, but management disputes its assumptions.

HG Singapore immediately licensed the IP back to HG Australia for the greater
of 4,000,000 per year or 12% of Australian revenue. HG Australia accrued a
4,000,000 royalty at 30 June 2025 but had not paid it when advice was requested.
No royalty withholding amount has been remitted. Internal board papers describe
these objectives:

- centralise global IP ownership before an offshore capital raising;
- give Singapore sales staff a commercially credible operating entity; and
- “move future platform profit out of the Australian 25% tax net”.

The transferred IP remains hosted and developed principally by the Australian
engineering team. HG Singapore can request roadmap changes but had not done so
by 15 July 2025. No formal development, enhancement, maintenance, protection,
or exploitation (`DEMPE`) analysis was prepared. No transfer-pricing
documentation was completed before lodgment work began.

## Required advice

Prepare a tax-risk memorandum for HG Australia's board. It must:

1. analyse, rather than merely list, the likely treatment of each R&D activity,
   expenditure item, timing issue, grant, and prototype receipt under the R&D
   tax incentive;
2. calculate a defensible provisional R&D notional-deduction and offset range,
   showing assumptions and separately identifying feedstock, recoupment/clawback,
   associate-payment, overseas-activity, and mixed-use issues;
3. analyse the IP assignment and licence-back, including ordinary deduction or
   capital treatment, market-value/arm's-length consequences, transfer pricing,
   royalty withholding, residence implications for HG Singapore, and the
   potential application of Part IVA;
4. identify the most important evidence, registrations, valuations,
   contemporaneous records, and governance steps the group should obtain before
   lodgment;
5. distinguish legislation, ATO public guidance/rulings, and judicial authority,
   and state the weight and limits of each source;
6. include contrary arguments and uncertainty—especially where the facts do not
   permit a concluded view;
7. cite stored official URLs and typed `DocumentId`/`ChunkRef` values for every
   material proposition, and quote only the minimum necessary text; and
8. finish with a prioritised risk table and a list of questions for management.

## Research protocol

Use only the remote MCP server named `australian-legal-remote`. Do not use the
legacy local ATO server, web search, remembered citations, or unofficial
summaries.

- Select exactly one source for every search.
- Start with focused keyword searches; use hybrid/vector search only where it
  adds value.
- Read the returned chunks and relevant neighbours before relying on a hit.
- Use `get_doc_anchors` to navigate long authorities and locate related or
  citing material.
- Use `get_definition` for statutory terms where useful.
- Use `fetch` only for canonical ATO legal URIs; other sources are retrieved
  from the indexed corpus.
- Prefer current legislation/guidance, but use `include_old=true` where a still
  relevant older ruling or case would otherwise be excluded.
- Do not silently broaden a source search or treat an ordinary-meaning fallback
  as a statutory definition.

This is a research exercise, not legal or tax advice.
