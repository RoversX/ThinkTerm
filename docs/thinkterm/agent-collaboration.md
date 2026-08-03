# Agent Collaboration in ThinkTerm

**Status:** Exploratory proposal

**Decision:** Not yet adopted

## Summary

ThinkTerm's mux architecture makes an unusual form of agent collaboration
possible today: an AI agent running in one terminal pane can send input to an
agent in another pane, observe its output, and use the result to continue its
own work.

This has been demonstrated using two existing CLI primitives:

- [`thinkterm cli send-text`](../cli/cli/send-text.md) sends bytes to a target
  pane as if they had been typed or pasted by the user.
- [`thinkterm cli get-text`](../cli/cli/get-text.md) reads the visible or
  scrollback contents of a target pane.

Together, these commands form a minimal communication loop:

```text
Agent A
  -> send-text
  -> Agent B's terminal input
  -> Agent B performs work
  -> Agent B prints a response
  -> get-text
  -> Agent A receives the result
```

The experiment is useful because it requires no agent-specific integration.
Codex, Claude Code, a shell script, or any other interactive terminal program
can participate. However, terminal input and screen scraping are not a safe or
reliable long-term protocol. This document records what was demonstrated and
outlines a possible native design that ThinkTerm may adopt later.

## The Demonstrated Experiment

Claude Code was running in one ThinkTerm thread and Codex was running in a
second thread. Claude first used the mux CLI to discover the target pane and
inspect its visible output. It then sent a correction to Codex with
`send-text --no-paste`, followed by Enter as a separate input operation.

Codex received the message exactly as though the user had typed it. It ran a
new investigation, corrected its earlier conclusion, and printed a response.
Claude polled the pane with `get-text`, detected the completed response, and
reported the result back to the user.

The investigation itself was a useful example of collaboration. Codex had
initially failed to find a running mux server because its process-list command
was executed inside an isolated PID namespace. Claude, which had evidence from
the host environment, challenged that conclusion. Codex then inspected the
host process list and confirmed the active `wezterm-mux-server` process.

In other words, one agent:

1. Observed another agent's result.
2. Identified a likely mistake.
3. Sent a follow-up task into the other agent's pane.
4. Caused that agent to perform a second investigation.
5. Read the corrected response from the terminal.

This completed a real agent-to-agent feedback loop using only terminal and mux
primitives.

## What the Experiment Proves

### A pane can act as an agent runtime

A ThinkTerm pane already provides process execution, input, output, scrollback,
remote connectivity, and persistence. An agent does not need to be embedded in
ThinkTerm to benefit from those capabilities.

### The mux can act as a communication fabric

Because the mux identifies panes across windows, threads, and connected
clients, a task can be delivered without depending on the visual location of
the target terminal. The same model can potentially work across devices when
both clients share a mux domain.

### Existing tools are enough for prototyping

The experiment did not require a new protocol or changes to Codex or Claude
Code. That makes `send-text` and `get-text` useful for quickly exploring agent
workflows before committing to a larger architecture.

### Terminal access is a powerful trust boundary

To an interactive application, injected bytes are indistinguishable from
physical keyboard input. A process that can control the mux may be able to
issue prompts, approve actions, interrupt programs, or run shell commands in
other panes. This is useful, but it must be treated as control of the user's
terminal rather than as harmless messaging.

## Limitations of the Current Approach

The demonstrated loop should be considered a prototype, not an application
protocol.

### No message identity or provenance

The receiving agent cannot tell whether input came from the user, another
agent, a script, or a compromised local process. The terminal byte stream has
no trusted sender identity or metadata.

### Screen output is not a structured response

`get-text` returns terminal contents, not a task result. Callers must infer
whether the agent is still working, waiting for permission, displaying an old
answer, or finished. Full-screen TUI redraws make this inference even more
fragile.

### Completion requires polling

There is no native event indicating that an agent accepted or completed a
task. A controller must repeatedly read the pane and compare output, which is
inefficient and can produce false positives.

### Input behavior depends on the application

Some applications distinguish keyboard input from bracketed paste. Some need
the prompt and Enter to be delivered separately. Input can also arrive while a
program is in a modal dialog, confirmation prompt, search field, or alternate
screen.

### Authorization is indirect

Current permission systems may approve the command that calls `send-text`, but
they do not necessarily express the more important intent: one agent is being
allowed to control another interactive session.

### Terminal text is a poor format for large context

Pasting diffs, logs, or source files through a terminal is slow and may lose
structure. References to files and artifacts are usually more appropriate than
embedding all content in a prompt.

## A Possible Native Model

If ThinkTerm adopts agent collaboration as a product feature, it should use a
structured mux channel while keeping terminal injection as an explicit
compatibility fallback.

### Agent registration

An agent-capable pane could advertise a small set of metadata:

```text
agent_id
agent_kind          (Codex, Claude Code, custom, unknown)
pane_id
workspace/thread
capabilities
state               (ready, working, waiting, finished)
```

Automatic process detection may help the UI, but trusted registration should
not rely only on process names or terminal titles.

### Structured task messages

A task envelope could contain:

```text
task_id
source
target
prompt
attachments
requested_capabilities
created_at
```

Responses could be represented as lifecycle events:

```text
TaskAccepted
TaskProgress
TaskWaitingForApproval
TaskCompleted
TaskFailed
TaskCancelled
```

Task identifiers would allow multiple requests to share a pane without
confusing new output with an earlier response.

### Capability-based authorization

Permissions should describe the action being granted, not merely the command
used to implement it. Possible capabilities include:

- Discover agent panes.
- Read another pane's terminal output.
- Send a task to an agent.
- Inject raw terminal input.
- Attach files or diffs.
- Cancel a task.
- Approve a privileged action on behalf of the user.

The last capability should normally remain unavailable. Sending a task must
not implicitly grant an agent permission to approve destructive or privileged
operations requested by another agent.

### Observable provenance

ThinkTerm should visibly distinguish user input from agent messages. The UI
might show a small event such as:

```text
Task received from Claude Code in Thread 4
Approved by the user
```

An audit record should capture the sender, target, task ID, permission decision,
and outcome. Sensitive prompt or output content may require optional redaction.

### Attachments and shared artifacts

Tasks should be able to reference repository files, patches, command output, or
other artifacts without reproducing them through terminal input. A reference
must include enough host and workspace identity to prevent a path from one
machine being interpreted on another machine.

## Possible MVP

A small first version could deliberately avoid general orchestration. It would
need only:

1. Agent discovery and a visible ready/working state.
2. A structured task with a unique ID, source, target, and prompt.
3. Accepted, waiting, completed, failed, and cancelled events.
4. A per-task user permission prompt before cross-pane control.
5. A simple **Send Task to Agent** action in the thread or pane UI.
6. An audit trail that clearly distinguishes agent messages from user input.

For agents without a native integration, ThinkTerm could still translate a
structured task into `send-text`. Such tasks should be labeled as terminal
injection, and completion detection would remain best-effort.

## Non-Goals for an Initial Version

An initial implementation would not need to:

- Decide automatically which agent is best for every task.
- Build a general distributed job scheduler.
- Allow agents to bypass their own permission systems.
- Interpret arbitrary terminal output as trusted structured data.
- Support invisible background control of user sessions by default.
- Replace the terminal as the primary way users interact with their agents.

The goal would be reliable, understandable handoff between user-selected agent
sessions, not autonomous control of the entire workstation.

## Open Questions

- Should agent messages be part of the existing mux protocol or use a separate
  local service?
- How does an external CLI agent register without requiring vendor-specific
  changes?
- Which state belongs to a pane, a thread, a workspace, or an agent process?
- What happens when an agent exits and restarts in the same pane?
- How are tasks recovered after a client or mux-server reconnect?
- Can multiple clients safely observe or control the same agent?
- Which permissions are session-scoped, workspace-scoped, or persistent?
- How should ThinkTerm present agent activity without turning the terminal UI
  into a full project-management interface?
- What is the safest compatibility strategy for agents that only accept
  terminal input?

## Adoption Criteria

This proposal should be considered for implementation only if the structured
model provides clear value beyond scripts built from `send-text` and
`get-text`. Before adoption, a design should demonstrate that it can:

- Preserve user control and make message provenance obvious.
- Avoid granting new implicit authority to local processes or other agents.
- Work across reconnects without duplicating or losing tasks.
- Support both local and remote mux domains safely.
- Fail visibly when the target agent cannot accept structured messages.
- Remain useful with more than one agent implementation.

## Conclusion

The experiment shows that ThinkTerm already contains the basic ingredients for
agent collaboration. A pane can host an agent, the mux can locate it, and
existing CLI commands can create a complete request-and-response loop.

That is enough for compelling demos and personal automation. It is not yet
enough for a trustworthy product feature. If ThinkTerm adopts this direction,
the next step should be a small structured task protocol with explicit identity,
permissions, lifecycle events, and visible provenance—not increasingly complex
screen scraping and keyboard injection.
