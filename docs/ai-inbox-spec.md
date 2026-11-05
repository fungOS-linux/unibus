# AI inbox: what a mail client must do

Status: product/architecture spec. Written 2026-11-05 after reviewing Superhuman
Mail/AI and Macro's public product pages. It is not a commitment to copy either
product; it is the bar an AI-native client should meet.

## 1. The premise

An AI inbox is not a chat sidebar bolted onto a list of messages. It is an inbox
that continuously answers three questions:

1. **What needs me now?** Triage, priority, and a short reason.
2. **What does this mean?** Thread summary, decisions, commitments, entities.
3. **What should I do next?** A draft, a reminder, an event, a task, a record
   update — with citations and an approval step.

The client still has to be an excellent mail client. AI cannot rescue a slow,
keyboard-hostile, unreliable inbox.

## 2. Table stakes (non-AI)

Observed across Superhuman Mail/AI and expected by anyone leaving Gmail/Outlook:

| Capability | Requirement |
| --- | --- |
| Speed | Open, archive, next/previous, and search are effectively instant; keyboard-first with a command palette. |
| Triage | Split inbox / tabs by VIP, team, tool, or custom rule. |
| Time control | Snooze, Send Later, Undo Send, reminders, follow-up reminders. |
| Composition | Snippets/templates, attachments, CC/BCC, signatures, plain-text fallback. |
| Threads | Correct threading, quoted text collapse, per-message actions. |
| Awareness | Read status / social insights (where provider allows), unsubscribe. |
| Calendar | See availability, propose times, turn an email into an event. |
| Collaboration | Share a live thread, comment internally, assign follow-ups. |
| Search | Fast full-text + metadata search, offline-tolerant. |
| Multi-account | Distinct identities, signatures, send-as, and per-account policy. |

If any of these are missing, the AI layer is noise.

## 3. AI capabilities

### 3.1 Triage and organization

- **Auto-label** from short natural-language rules ("job applications",
  "requests to review work") and from learned sender/tool patterns.
- **Auto-archive** low-value classes (marketing, cold outreach, social) with an
  explicit allowlist and one-click undo.
- **Split inbox** driven by those labels, not only hardcoded tabs.
- **Priority** with a visible reason ("VIP sender", "direct question", "deadline
  in 2 days", "thread you started").
- **Explainability**: every AI placement can answer "why am I seeing this?" and
  can be corrected; corrections feed the rule, not a black box.

### 3.2 Understanding

- **One-line thread summary**, updated as messages arrive; expand for:
  - decisions made,
  - open questions,
  - commitments and owners,
  - deadlines,
  - entities (people, companies, deals, documents).
- **Action extraction**: "reply by Friday", "send the deck", "approve invoice".
  Proposed tasks are drafts, never silently created.
- **Ask AI** across mail, calendar, and linked records: natural-language search
  with citations to the exact message/record. "Where's the offsite?" must link
  the message, not invent an answer.
- **Daily brief**: a short list of what changed and what needs action, grouped by
  urgency, each item expandable to its source.

### 3.3 Writing

- **Draft in my voice** using sent mail as style evidence, with tone/length
  controls and a diff against the source thread.
- **Instant reply** for common cases; always editable before send.
- **Write with voice** (speech to polished text) on mobile/desktop.
- **Citations in drafts**: each factual claim links to the message or record it
  came from. A draft that cannot cite a claim must flag it.
- **No auto-send by default.** Agentic send requires an explicit policy,
  recipient class (internal vs external), approval, and audit.
- **Prompt-injection resistance**: email bodies are untrusted input. Instructions
  found inside a message never become tool calls.

### 3.4 Acting

- Follow-up detection → reminder → pre-written follow-up.
- Meeting proposal → event with title, description, location, attendees, and
  availability-aware time.
- Task/commitment → task in the workspace, linked back to the thread.
- CRM update → proposed field changes with a diff and approval.
- Workflows → multi-step actions with a dry-run, approval gates, and rollback.

## 4. The killer composition features

### 4.1 @-mention to attach or link (Macro-style)

Typing `@` opens one palette across:

- files (local, cloud docs, pasted artifacts),
- contacts/people,
- sheets/databases and specific rows,
- CRM companies/deals,
- prior emails/threads.

Behavior:

- **Link by default, copy on request.** Inserting a doc link keeps permissions and
  freshness; "attach a copy" is a separate, explicit action with size/type
  warnings.
- **Permission-aware.** The palette only shows what the sender may share. A
  mention never silently broadens access.
- **Preview before send**: name, type, size, owner, and whether the recipient can
  open it.
- **Stable references**: the draft stores a typed reference (file id, record id),
  not a rendered URL that rots.
- **Audit**: who attached what, from where, and with which permission change.

### 4.2 CRM auto-mailmerge (later, high value)

- Templates bind typed fields to CRM records: `{{contact.first_name}}`,
  `{{deal.amount}}`, `{{company.renewal_date}}`.
- **Preview per recipient** with a diff; unresolved fields block send.
- **Permissions and scope**: the sender can only read the records they can read;
  bulk send is rate-limited and approval-gated.
- **Audit**: template version, record revisions, rendered output hash, sender,
  recipients, approval.
- **No silent data exfiltration**: merging a record does not attach the record.
- **Compliance**: unsubscribe, suppression lists, bounce handling, and per-domain
  limits are part of the merge pipeline, not an afterthought.

### 4.3 Contacts, People, Sheets

- One canonical **person/company** entity, resolved from sender/recipient
  headers, CRM, and directory. Merge duplicates with a review step.
- Contact cards show: last thread, open commitments, deal/record context,
  recent docs, and "do not contact" state.
- Sheets/databases are first-class sources: a row can be referenced, previewed,
  and merged, with the same permission and audit rules as CRM records.
- Never let AI invent a contact or a field. Unknown means unknown.

## 5. Trust, safety, privacy

- **No training on customer mail.** Contractual prohibition, per-tenant isolation,
  and a documented retention window.
- **Model routing is policy**: which provider/model sees which content, what is
  redacted, and whether a local-only mode exists.
- **Minimum necessary context**: send the thread slice needed for the task, not
  the whole mailbox.
- **Prompt injection**: treat message content, attachments, and linked docs as
  hostile. Tool calls require an allowlist and an approval boundary.
- **Citations and confidence**: every summary/answer links sources; low confidence
  is shown, not hidden.
- **Audit**: AI actions, approvals, sends, permission changes, and CRM writes are
  recorded; the user can see "what did the agent do?".
- **Kill switch**: disable AI per account, per label, or per recipient class.
- **Secrets**: credentials live in geode; models and prompts never receive them.

## 6. Architecture for Unibus / Soil / Spore / Canvas

The mail client is a Canvas application; the AI layer sits above the bus. The bus
stays metadata-only.

```text
mail/chat adapters ── email.* / chat.* metadata ──▶ Unibus
        │                                              │
        └── bounded fetch (content, attachments) ◀─────┘
                                                       │
Canvas UI ◀── AI inbox service ──▶ Soil/Spore records (contacts, CRM, docs)
                                                       │
                                                 model providers
```

Rules:

- **Read path**: `email.message.v1` / `chat.message.v1` metadata, `*.delta.v1`
  windows, `*.fetch.v1` for bounded content. No bodies on the bus.
- **Write path**: drafts and sends are separate contracts and separate grants.
  `chat.send` is already a distinct capability; mail needs `email.draft`,
  `email.send`, and `email.fetch.reply` equivalents.
- **Attachments and documents** are references, not bytes. A resolver checks
  permission, returns metadata and a bounded stream on request. The draft stores
  the reference; the renderer resolves at send time.
- **Contacts/CRM/Sheets** are Soil/Spore records with WIT-defined capabilities,
  not a second silo inside the mail adapter. The mail client queries records via
  their owner, with the user's authority.
- **AI service** consumes metadata + explicitly fetched content + records. It
  never gets a credential, and it never writes to a record or sends mail without
  an approval boundary.
- **Canvas** owns presentation: split inbox, thread view, composer, record side
  panel, and the approval UI. WIT contracts should describe capabilities, not
  pixels.
- **Offline**: metadata cache + queued drafts; no silent send on reconnect.

### Contracts still missing

| Contract | Why |
| --- | --- |
| `email.thread.v1` | Thread identity and participant set beyond a single message. |
| `email.draft.v1` / `email.send.v1` | Create/edit/send with explicit grants, approval, audit. |
| `email.attachment.v1` | Attachment metadata + bounded content fetch, never inline bytes. |
| `contact.v1` / `crm.record.v1` (or Spore equivalents) | Canonical people/companies/deals for @-mention and merge. |
| `document.ref.v1` (or Spore) | Stable file/doc reference with permission check. |

## 7. UX shape

- **Keyboard-first three-pane**: list, thread, context panel.
- **Top of list**: a compact AI brief ("3 need replies, 1 deadline, 1 deal
  update"), each item expandable to its source.
- **Thread header**: one-line summary + chips for decisions, questions, deadlines.
- **Composer**: recipient chips, @-mention palette, merge-field chips, citation
  popovers, "why this draft?" panel, approval/send split.
- **Side panel**: person/company, CRM record, recent docs, open commitments.
- **Command palette**: "snooze until Monday", "draft a decline", "merge renewal
  template for Acme", "attach the rollout plan".
- **Every AI action** has undo for the reversible window and a visible audit
  trail after it.

## 8. V1 vs later

**V1 (read + assist):**

- table stakes mail, multi-account
- auto-label + split inbox with user rules
- thread summary with citations
- instant reply / draft in voice, always editable
- follow-up reminders
- Ask AI over mail + calendar with citations
- @-mention link (not copy) for docs and contacts
- contact card with recent thread context

**Later:**

- CRM auto-mailmerge with per-recipient preview and audit
- agentic workflows with approval gates
- shared team thread views and comments
- Sheets/database row merge
- local-only model mode and per-tenant routing

**Non-goals:**

- autonomous external send without an explicit policy and approval
- training on customer mail
- putting message bodies, attachments, or credentials on the bus
- a second CRM/contacts silo inside the mail adapter

## 9. Open decisions

1. **Model boundary**: cloud-first with tenant isolation vs local-first for
   sensitive accounts. Can be per-account policy.
2. **Record ownership**: Soil/Spore vs CRM sync. Pick one canonical store before
   building merge.
3. **Attachment semantics**: link-by-default with permission check, or copy with
   explicit consent. Recommendation: link by default.
4. **Send approval**: per-recipient-class policy (internal auto, external
   approval) vs always manual. Recommendation: external always manual in v1.
5. **Retention**: how long fetched content and embeddings live; must be shorter
   than mail retention and auditable.

## 10. References

- Superhuman Mail: <https://superhuman.com/mail> — split inbox, snippets, send
  later, read status, shared threads/comments, calendar.
- Superhuman AI: <https://superhuman.com/ai> — auto labels/archive, one-line
  summaries, instant reply, write with voice, follow-up reminders, Ask AI,
  workflows, privacy claims.
- Macro: <https://macro.com/> — one inbox with email/chat/tasks/docs/CRM,
  @-mention agents and files, records created from email, CRDT docs, Sheets.
