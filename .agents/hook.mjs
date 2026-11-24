import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

function readStdin() {
  try {
    return fs.readFileSync(0, 'utf8');
  } catch {
    return '';
  }
}

function getBranch(cwd) {
  if (!cwd) return 'unknown';
  try {
    return execFileSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], {
      cwd,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
      timeout: 1500,
    }).trim() || 'unknown';
  } catch {
    return 'unknown';
  }
}

try {
  const raw = readStdin().trim();
  if (raw) {
    const payload = JSON.parse(raw);
    const tc = payload.toolCall || {};
    const args = tc.args || {};
    const toolName = String(tc.name || args.ToolName || args.tool_name || '');
    const isDelegate = toolName.includes('delegate_task');
    const isFollowup = toolName.includes('t3_thread_send');

    if (isDelegate || isFollowup) {
      const promptText = String(isDelegate ? (args.task ?? '') : (args.message ?? ''));
      const verMatch = promptText.match(/\[handoff:(v\d+)\]/i);
      const devHome = process.env.SINGULARITY_DEV_HOME || path.join(os.homedir(), '.singularity-dev');
      fs.mkdirSync(devHome, { recursive: true });
      const entry = {
        ts: new Date().toISOString(),
        type: 'handoff',
        tool: isDelegate ? 'delegate_task' : 't3_thread_send',
        repo: payload.cwd ? path.basename(payload.cwd) : 'unknown',
        branch: getBranch(payload.cwd),
        title: args.title || null,
        role: args.role || null,
        target: args.target || null,
        threadId: args.threadId || null,
        handoff_version: verMatch ? verMatch[1].toLowerCase() : null,
        prompt: promptText,
        error: payload.error || null,
      };
      fs.appendFileSync(path.join(devHome, 'ledger.jsonl'), JSON.stringify(entry) + '\n', 'utf8');
    }
  }
} catch {
  // Silent telemetry logger: never block or fail the agent loop.
}

process.stdout.write('{}\n');
