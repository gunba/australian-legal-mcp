# HarbourGrid memorandum — substantive technical review

- **Reviewed:** 21 July 2026
- **Underlying fictional advice date:** 15 July 2025
- **Original research generation:** Arroy v20 `a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`
- **Review generation:** flat-int8 v22 `937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`
- **Disposition:** useful research output, but **not approved as a board-ready memorandum without the corrections below**.

This is quality assurance for a fictional retrieval exercise, not legal or tax
advice. The review used only the authenticated remote Australian Legal MCP. It
did not use web search, the legacy ATO service, or unofficial summaries.

## Overall assessment

The memorandum finds the right major risk areas and is strongest on activity
eligibility, the overseas finding, group mark-ups, feedstock, transfer pricing,
residence by central management and control, withholding, and Part IVA. Its
arithmetic is internally consistent. It also correctly avoided the superseded
one-third feedstock adjustment.

The document is not yet board-ready because it presents illustrative R&D
allocations as a defensible range, does not establish all conditions for the
43.5% refundable rate, overstates what the facts show about the same-day royalty
and DEMPE, omits a direct application of the resident-shareholder voting-control
limb, and does not separate mutually exclusive residence/consolidation/
withholding scenarios. Its generation-specific citations also no longer resolve
through the current public interface.

## R&D review

### What is technically sound

- The proposed $13.6 million claim should not be accepted from ledger labels.
- Singapore activity is excluded on the stated facts because no overseas
  finding was obtained. The $850,000 underlying cost is only a counterfactual
  cost limit before eligibility, allocation, payment-timing, and mark-up tests.
- Interest is excluded from Division 355, without deciding its treatment under
  another provision.
- If the founder-controlled contractor is an associate, payment of $400,000 by
  year end limits the current-year timing amount; payment does not prove that
  the work was eligible.
- The standalone feedstock arithmetic is correct only if the sold units are the
  relevant feedstock or transformed feedstock output, the $1.2 million proceeds
  equal market value, the output is itself the marketable product (or the
  statutory output/product cost ratio is applied), and the $500,000 production
  cost reconciles to attributable notional deductions:

  ```text
  provisional feedstock base = min($1.2m feedstock revenue, $0.5m attributable deductions)
                             = $0.5m
  assessable inclusion at a fixed 43.5% offset and 25% company rate
                             = $0.5m × (43.5% - 25%) / 25%
                             = $0.370m
  ```

- A $3 million reimbursement entirely referable to eligible notional
  deductions would produce a $2.220 million clawback inclusion under the same
  fixed-rate assumptions.
- The memorandum correctly warns that grant and feedstock amounts cannot be
  added where they claw back the same notional deductions.

### Required corrections

1. **Make the 43.5% rate conditional.** Turnover below $20 million and a 25%
   company rate support `25% + 18.5%`. The case's statement that HG Australia
   has no exempt income does not establish that no exempt entity or combination
   of exempt entities controls it. If that condition fails, expenditure
   intensity and the non-refundable rules require a different calculation.

2. **Rename the $6.530m and $10.015m figures.** Their arithmetic is correct, but
   they are illustrative conservative and favourable scenarios—not a
   defensible minimum and evidentiary ceiling. In particular:
   - 60% salary and 50% cloud allocations are unsupported haircuts;
   - the lower scenario assumes all $400,000 paid to the contractor was for
     eligible work;
   - the favourable scenario assumes all Australian salary expenditure was
     eligible despite known routine/customer work; and
   - the pilot and any separately identified eligible legal/technical work
     could make the stated figure an incomplete ceiling.

3. **Correct the pilot explanation.** The facts describe nine months of
   experimental use followed by commercial redeployment, not concurrent mixed
   use. If $900,000 is full-year decline in value and the first nine months were
   wholly eligible, $675,000 is the conditional amount. If $900,000 already
   represents decline during that period, the result may differ. Tax cost,
   method, start date, period represented by the ledger entry, and actual use
   are blockers.

4. **Keep, but formulate, the project-grant sensitivity.** A project recoupment
   is not always capped at the cash grant. Its statutory cap is:

   ```text
   net recoupment × R&D expenditure / project expenditure
   ```

   The memorandum's whole-premium scenario is legally possible for a matching
   or mandated-expenditure grant, but the facts establish only that the grant
   was tied to the project. The deed must identify the required project
   expenditure, relevant R&D expenditure, repayments, and traced notional
   deductions. The $4.832m–$7.411m inclusions must therefore be labelled
   contingent sensitivities, not likely exposure.

5. **Do not describe the 25% component as an ordinary deduction in every case.**
   It is the company-rate component of the offset calculation. The underlying
   expenditure may be revenue, capital, or decline in value, and may not have
   produced an immediate ordinary deduction outside Division 355.

6. **Keep gross offset separate from cash.** A gross conditional offset does not
   establish the refund. The final tax loss, refundable-status gateway,
   clawback inclusions, other liabilities, and account balances are unknown.

## IP assignment, royalty, residence, and Part IVA review

### Same-day royalty

The case establishes a licence on 30 June, a greater-of annual price, a $4
million accounting accrual, and no cash payment by 15 July. It does **not**
establish that the whole annual minimum became incurred or payable on execution.
The sentence that HG Australia “must immediately pay” $4 million should be
removed.

Before claiming a 2024–25 deduction, determine:

- commencement time and first royalty period;
- whether the minimum is upfront, in arrears, progressive, or prorated;
- when the 12% alternative is measured;
- the condition that creates the liability;
- whether the entry was only a provision or gave Singapore an unconditional
  right; and
- whether the amount is wholly a royalty, partly services or distribution
  consideration, or economically deferred assignment consideration.

The tax amount for 2024–25 could be nil, prorated, or the full minimum depending
on the executed terms and character. A journal entry alone does not decide it.

### Payment, withholding, and section 26-25

The analysis must keep four questions separate: incurrence, actual or deemed
payment, the withholding obligation, and the remittance due date. Non-payment by
15 July does not itself prove default. An unconditional credit may nevertheless
be deemed payment even without cash movement.

The memorandum's section 26-25 conclusion is substantively correct: if the
royalty was otherwise deductible for an income year, subsection (3) permits the
deduction for that income year once the relevant withholding tax is paid. An
amendment may be needed if that year has already been assessed.

The 10% treaty and 30% domestic figures are full-$4-million sensitivities only.
Ten per cent applies only to the qualifying treaty-protected royalty amount
after residence, beneficial ownership, PE connection, PPT, and the
special-relationship limitation are resolved. A non-arm's-length excess cannot
simply inherit the treaty cap.

### Residence is a gateway, not an additive exposure

The central-management-and-control analysis is strong but incomplete. HG
Singapore is wholly owned by an Australian-incorporated shareholder. Subject to
its constitution, share classes, and voting rights, the resident-shareholder
voting-control limb is independently material if HG Singapore carries on
business in Australia.

The final analysis should use mutually exclusive scenarios:

| Scenario | Principal consequences to resolve |
|---|---|
| Australian resident and member of an existing consolidated group | Consolidation may change or eliminate the intragroup disposal/licence consequences. |
| Australian resident but not consolidated | Worldwide-income and domestic related-party consequences; non-resident royalty withholding may not apply. |
| Non-resident and treaty-resident in Singapore | Division 815, royalty withholding, treaty entitlement, PPT, PE, and beneficial ownership become central. |

These outcomes must not be added as if all apply together.

### Asset, DEMPE, valuation, and specific-rule sequencing

- The facts support principal Australian development/hosting and Perth control
  of specified strategy. They do not prove the complete development,
  enhancement, maintenance, protection, exploitation, financing, and risk-
  control position.
- The transfer occurred in anticipation of an offshore capital raising; the
  date of a raising is not given.
- Trade marks, goodwill, customer relationships, and contractual rights should
  be investigated, not assumed to be part of “related rights”.
- The $23 million difference is a gross valuation discrepancy. Multiplying it
  by 25% gives a sensitivity, not assessed tax.
- Calculation must proceed asset by asset: identify and allocate; classify under
  Division 40, CGT, or another regime; determine termination value or capital
  proceeds; trace R&D results; calculate section 355-410; apply anti-overlap;
  then apply Division 815 as a substitution of arm's-length conditions rather
  than stacking a second charge on the same difference.

### Part IVA

The current facts support a high **Part IVA investigation risk**, not a concluded
application. The final memorandum should map each scheme to a tax benefit,
income year, reasonable alternative, section 177D factors, and interaction with
residence and the specific rules. A 2024–25 royalty benefit exists only if the
royalty was incurred in that year. Future profit shifting concerns later years.
The profit-shifting board paper is strong Part IVA evidence but does not by
itself prove that obtaining the treaty's 10% withholding cap was one principal
purpose for the PPT.

## Corrected ordering of board risks

1. Residence, treaty status, and consolidation — critical gateway; unquantified.
2. IP asset character and arm's-length value — critical; $23m gross discrepancy.
3. Royalty incurrence and character in 2024–25 — critical; amount not established.
4. Actual/deemed payment and withholding — critical but conditional rate sensitivities.
5. Transfer pricing and continuing Australian functions — critical in the applicable cross-border scenario.
6. Part IVA — high/critical investigation risk, not additive to specific-rule adjustments.
7. R&D activity substantiation and allocation — high; scenario range not a claim range.
8. Grant terms and project-expenditure formula — high; deed is a calculation blocker.
9. Overseas activity and associate timing — high but clearer exclusions/conditions.
10. Feedstock, pilot decline in value, and legal expenditure — conditional tracing and character issues.

## Citation and point-in-time disposition

The original source register is valid historical provenance, but its v20
`ChunkRef` values are not reproducible through the current public v22
`get_chunks`: the server rejects them as belonging to the wrong generation.
The final memorandum must preserve the historical v20 audit snapshot and add
v22 successor references (or preserve documented serving access to v20). It
should also add legal pinpoints—subsection, treaty
article, ruling paragraph, and judgment paragraph—rather than relying only on
chunk IDs.

The FRL URLs use mutable `/latest/text` paths. The review did not establish the
compilation in force on 30 June or 15 July 2025. Point-in-time verification is a
remaining legal-research requirement. The treaty conclusions also rely on an
ATO synthesis that disclaims primary-authority status; authentic treaty, MLI,
and implementing provisions are required before definitive advice.

## V22 official-source evidence used for this review

All references below use generation
`937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`.

- Refundable-rate gateway and exempt-entity control:  
  `DocumentId{source:"frl", native_id:"C2004A05138"}`;  
  `ChunkRef{source:"frl", generation:"937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939", chunk_id:5829893}`.  
  <https://www.legislation.gov.au/C2004A05138/latest/text>
- Expenditure, associate payment timing, and overseas finding:  
  same `DocumentId`; chunks `5829899`–`5829901`.  
  Same URL.
- Excluded interest and decline in value:  
  same `DocumentId`; chunks `5829905`–`5829907`.  
  Same URL.
- Group mark-up reduction:  
  same `DocumentId`; chunk `5829917`.  
  Same URL.
- Government project recoupment cap, including the preserved formula asset:  
  same `DocumentId`; chunks `5829921`–`5829922`.  
  Same URL.
- Feedstock amount and revenue formula asset:  
  same `DocumentId`; chunks `5829923`–`5829924`.  
  Same URL.
- Royalty deduction after withholding-tax payment:  
  same `DocumentId`; chunk `5826780`.  
  Same URL.
- Corporate residence and resident-shareholder voting control:  
  `DocumentId{source:"frl", native_id:"C1936A00027"}`;  
  `ChunkRef{source:"frl", generation:"937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939", chunk_id:5729500}`.  
  <https://www.legislation.gov.au/C1936A00027/latest/text>
- Uniform-clawback explanatory material:  
  `DocumentId{source:"ato", native_id:"NEM/EM202033/NAT/ATO/00007"}`;  
  v22 chunks `853717`–`853718` contain the matching-grant Example 5.1; chunk
  `853751` contains paragraph 5.64's later clarification of the recoupment-cap
  numerator.  
  <https://www.ato.gov.au/law/view/document?docid=NEM/EM202033/NAT/ATO/00007>

## Final disposition

Retain the original memorandum as evaluation evidence. Do not present it as
concluded board advice. A revised memorandum should incorporate the corrections
above, retain the historical v20 audit snapshot, add v22 successor references,
and complete point-in-time and primary-treaty verification. The exercise
nevertheless demonstrates that the
server retrieved the decisive statutory rules, preserved the previously lost
formulas as typed assets, and exposed the factual questions needed for a
responsible answer.
