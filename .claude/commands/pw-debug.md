---
model: sonnet
description: Debug a Playwright E2E failure with full artifact capture (trace, video, screenshot) and auto-open the trace viewer.
---

# /pw-debug

Run a Playwright test (or a specific spec) with **maximum artifact capture** and interpret the failure block that `rtk playwright` emits.

## When to use

- A Playwright test is failing and you need to see what actually happened in the browser (video, screenshot, DOM state, network).
- A test is flaky and you want a deterministic single-worker run.
- The default `rtk playwright test` output showed the failure but not enough context to fix it.

## What this command does

1. Runs `rtk playwright debug $ARGUMENTS` — which expands to:
   ```
   playwright test --trace=on --video=on --screenshot=only-on-failure --workers=1 --retries=0 $ARGUMENTS
   ```
   The debug preset trades speed for signal: single worker, no retries, every artifact captured.

2. Reads the RTK-formatted failure block. It contains:
   - Error message + trimmed stack (node_modules frames pruned)
   - Snippet window (±2 lines around the failure marker)
   - Artifact paths: `video`, `screenshot`, `trace`
   - stdout tail (browser console errors, etc.)
   - **Debug helpers** section with copy-paste `npx playwright show-trace <path>` commands

3. For each failed test, opens the trace viewer by running the `show-trace` command from the helpers section. The trace viewer is authoritative — it has the DOM at every step, network requests, and console logs.

4. Correlates the failure to the code:
   - Reads the file identified in the stack (first non-node_modules frame)
   - Explains what the test expected vs what happened, referencing the trace timeline

## Usage

```
/pw-debug                                    # run entire suite in debug mode
/pw-debug tests/auth/login.spec.ts          # single spec
/pw-debug tests/auth/login.spec.ts:42       # single line
/pw-debug -g "should redirect"              # filter by test name
```

## Steps to follow

1. **Verify RTK is installed and version supports `debug`**:
   ```bash
   rtk --version   # need 0.42.5 or newer for `playwright debug`
   ```
   If missing or older, run `cargo install --path .` from the RTK repo root.

2. **Run the debug preset**:
   ```bash
   rtk playwright debug $ARGUMENTS
   ```

3. **Read the failure block**. The format is:
   ```
   PASS (N) FAIL (M)

   1. <test name> (<file>)
      Error: <message>
          at <first user frame>
      ── snippet ──
        > <line>
      ── artifacts ──
        video:      <path>
        screenshot: <path>
        trace:      <path>
      ── stdout (tail) ──
        <console logs>

   ── debug helpers ──
      npx playwright show-trace <trace.zip>
   ```

4. **For each failure, open the trace**:
   ```bash
   npx playwright show-trace <trace.zip path>
   ```
   Do this in a separate terminal — the viewer runs a local server.

5. **Read the source file** identified in the first user stack frame.

6. **Explain the failure** to the user:
   - What the test expected
   - What actually happened (from the trace)
   - Which line caused it
   - Suggested fix

## Interpretation guide

**"expect(page).toHaveURL(expected)" with `waiting for URL /X/`**: Playwright timed out waiting for navigation. Common causes:
- App threw before navigating (check stdout tail for uncaught errors)
- Redirect blocked (check network tab in trace)
- Auth/CORS failure (check response codes in trace)

**"strict mode violation: X resolved to Y elements"**: Selector matched multiple elements. Use `.first()` or narrow the selector.

**Snapshot-related failures**: Baseline differs from current. Trace shows both the actual and expected screenshots side-by-side.

**Timeout without specific error**: Usually the app is stuck loading. Check the trace's network tab for pending requests.

## Notes

- **Do NOT run without arguments in CI**. `--workers=1 --retries=0` is fine for local debugging but drastically slows a full suite. This command is for local investigation only.
- **The trace viewer works offline**. No need for internet after the initial `npx` fetch.
- **If `debug` isn't recognized**: RTK is outdated. Run `cargo install --path .` from the RTK checkout.
