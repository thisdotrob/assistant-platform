# assistant-approvals Contract

## Public API
Generic approval primitive: approval cards, pending-question/response matching, and expiry/sweep handling. Reused by any module needing human-in-the-loop confirmation.

## Persistence Ownership
Owns the pending-question and pending-approval state tables via central DB migrations.

## Config
Reads default approval expiry windows and sweep cadence from assistant-config; depends on assistant-permissions for who may approve.

## Events
Emits approval-requested, approval-granted, approval-denied, and approval-expired events.

## CLI/Web Surfaces
None directly; backs CLI pending-approval listing and web approval cards.

## Prompt Fragments
Owns `approval-policy`, which describes when to request approval and how approval cards are rendered and matched.

## Readiness Checks
Verifies the expiry sweep is running and no pending approvals are stuck past their expiry.

## Conformance Tests
Responses match the originating approval request; expired approvals are swept and never honored; only authorized approvers can grant.
