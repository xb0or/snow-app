import { dirname } from "node:path/posix";
import { processFileContent } from "../utils/fileReader";
import {
  connectSsh,
  deleteSshFile,
  disconnectSsh,
  executeSshCommand,
  listSshDirectory,
  parseSshUrl,
  readSshFile,
  readSshFileWithVersion,
  removeEmptySshDirectory,
  statSshEntry,
  isSshOperationError,
  toSshOperationErrorResult,
  writeInternalSshFile,
  writeSshFile,
  type SshConnectParams,
  type SshFileVersion,
  type SshFileWriteResult,
} from "./sshManager";
import { getDecryptedSecret, getSshCredential } from "./sshCredentials";

const REMOTE_SEARCH_MAX_DEPTH = 15;
const REMOTE_SEARCH_MAX_RESULTS = 200;
// Mirrors the local ripgrep timeout in native/src/mcp/servers/grep.rs so the
// SSH branch cannot hang the tool card forever when the remote side stalls.
const REMOTE_GREP_TIMEOUT_MS = 30_000;

export type RemoteWorkspaceCommand = {
  operation: string;
  argsJson: string;
};

type RemoteWorkspaceCommandArgs = {
  filePath?: unknown;
  startLine?: unknown;
  endLine?: unknown;
  searchContent?: unknown;
  replaceContent?: unknown;
  occurrence?: unknown;
  content?: unknown;
  overwrite?: unknown;
  pattern?: unknown;
  path?: unknown;
  fileGlob?: unknown;
  isRegex?: unknown;
  caseSensitive?: unknown;
  maxResults?: unknown;
  command?: unknown;
  workingDirectory?: unknown;
  timeout?: unknown;
  durable?: unknown;
  backend?: unknown;
  mode?: unknown;
  jobId?: unknown;
  offset?: unknown;
  limit?: unknown;
  workspaceId?: unknown;
  conversationId?: unknown;
  toolCallId?: unknown;
  workspaceRoot?: unknown;
  contentBase64?: unknown;
};

type RemoteWorkspaceSearchMatch = {
  file: string;
  line: number;
  content: string;
};

export const shellQuote = (value: string): string =>
  `'${value.replace(/'/g, `'"'"'`)}'`;

export const normalizeRemotePath = (path: string): string => {
  const normalized = path.replace(/\/+$/, "");
  return normalized || "/";
};

const validateSshWorkspacePath = (path: unknown, fieldName: string): string => {
  if (typeof path !== "string" || !path.trim().startsWith("ssh://")) {
    throw new Error(`${fieldName} must be an SSH workspace path`);
  }
  return path.trim();
};

const getRemotePathName = (path: string): string => {
  const normalizedPath = normalizeRemotePath(path);
  const separatorIndex = normalizedPath.lastIndexOf("/");
  return normalizedPath.slice(separatorIndex + 1) || "/";
};

const getRemoteRelativePath = (path: string, rootPath: string): string => {
  const normalizedPath = normalizeRemotePath(path);
  const normalizedRoot = normalizeRemotePath(rootPath);

  if (normalizedPath === normalizedRoot) {
    return ".";
  }
  if (normalizedRoot === "/") {
    return normalizedPath.replace(/^\/+/, "");
  }
  if (normalizedPath.startsWith(`${normalizedRoot}/`)) {
    return normalizedPath.slice(normalizedRoot.length + 1);
  }

  return normalizedPath.replace(/^\/+/, "");
};

export const buildRemoteWorkspaceUri = (
  workspacePath: string,
  remotePath: string,
  remoteRootPath: string
): string => {
  const relativePath = getRemoteRelativePath(remotePath, remoteRootPath);
  const normalizedWorkspacePath = workspacePath.replace(/\/+$/, "");

  return relativePath === "."
    ? normalizedWorkspacePath
    : `${normalizedWorkspacePath}/${relativePath}`;
};

export const buildSshConnectParams = (
  workspacePath: string
): SshConnectParams => {
  const parsed = parseSshUrl(workspacePath);
  const credential = getSshCredential(
    parsed.host,
    parsed.port,
    parsed.username
  );
  const connectParams: SshConnectParams = {
    host: parsed.host,
    port: parsed.port,
    username: parsed.username,
    authMethod: credential?.authMethod ?? "password",
  };

  if (credential?.privateKeyPath) {
    connectParams.privateKeyPath = credential.privateKeyPath;
  }

  const secret = credential?.encryptedSecret
    ? getDecryptedSecret(parsed.host, parsed.port, parsed.username)
    : null;
  if (secret) {
    if (connectParams.authMethod === "password") {
      connectParams.password = secret;
    } else {
      connectParams.passphrase = secret;
    }
  }

  return connectParams;
};

export const withSshSession = async <T>(
  workspacePath: string,
  action: (
    sessionId: string,
    remotePath: string,
    parsedPath: ReturnType<typeof parseSshUrl>
  ) => Promise<T>,
  options?: { signal?: AbortSignal }
): Promise<T> => {
  const parsedPath = parseSshUrl(workspacePath);
  const sessionId = await connectSsh(
    buildSshConnectParams(workspacePath),
    options
  );
  try {
    return await action(sessionId, parsedPath.remotePath, parsedPath);
  } finally {
    disconnectSsh(sessionId);
  }
};

const readTextFile = async (
  workspacePath: string,
  startLine: number | undefined,
  endLine: number | undefined,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const file = processFileContent(
        remotePath,
        await readSshFile(sessionId, remotePath, { signal })
      );
      if (file.isBinary || file.isImage) {
        throw new Error(
          "Remote filesystem edit operations require a text file"
        );
      }

      const lines = file.content.split("\n");
      const totalLines = lines.length;
      const requestedStart = Math.max(1, Math.floor(startLine ?? 1));
      const requestedEnd = Math.max(
        requestedStart,
        Math.floor(endLine ?? totalLines)
      );
      const selected = lines.slice(requestedStart - 1, requestedEnd);

      return {
        content: selected
          .map(
            (line, index) =>
              `${String(requestedStart + index).padStart(6, " ")}: ${line}`
          )
          .join("\n"),
        totalLines,
        startLine: requestedStart,
        endLine: Math.min(requestedEnd, totalLines),
      };
    },
    { signal }
  );
};

const resolveAuthorizedWorkspaceRoot = (
  workspacePath: string,
  workspaceRoot: unknown
): string => {
  const root = validateSshWorkspacePath(workspaceRoot, "workspaceRoot");
  const target = parseSshUrl(workspacePath);
  const authorized = parseSshUrl(root);
  if (
    target.host !== authorized.host ||
    target.port !== authorized.port ||
    target.username !== authorized.username
  ) {
    throw new Error(
      "workspaceRoot must use the same SSH authority as filePath"
    );
  }
  return authorized.remotePath;
};

const readRemoteText = async (
  workspacePath: string,
  signal?: AbortSignal
): Promise<{ content: string; version: SshFileVersion }> =>
  withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const loaded = await readSshFileWithVersion(sessionId, remotePath, {
        signal,
      });
      const file = processFileContent(remotePath, loaded.content);
      if (file.isBinary || file.isImage) {
        throw new Error(
          "Remote filesystem edit operations require a text file"
        );
      }
      return { content: file.content, version: loaded.version };
    },
    { signal }
  );

const writeRemoteText = async (
  workspacePath: string,
  workspaceRoot: string,
  content: string,
  expectedVersion: SshFileVersion,
  signal?: AbortSignal
): Promise<SshFileWriteResult> =>
  withSshSession(
    workspacePath,
    async (sessionId, remotePath) =>
      writeSshFile(sessionId, remotePath, content, {
        signal,
        workspaceRoot,
        expectedVersion,
      }),
    { signal }
  );

/**
 * Read the project ROLE.md from a remote SSH workspace.
 *
 * Mirrors RoleEditorPanel's SSH access path (`<remotePath>/ROLE.md`) so the
 * Rust prompt builder can inject the project role even for `ssh://`
 * workspaces. Returns `null` when the file does not exist, is binary, or SSH
 * is unavailable — callers then fall back to the global ROLE.md.
 */
export type RemoteRoleContext = {
  content: string | null;
  includeGlobalRules: boolean;
};

export const readRemoteRoleContext = async (
  workspacePath: string
): Promise<RemoteRoleContext> => {
  try {
    return await withSshSession(
      workspacePath,
      async (sessionId, remotePath) => {
        const projectRoot = remotePath.replace(/\/+$/, "");
        const rolePath = `${projectRoot}/ROLE.md`;
        let content: string | null = null;
        try {
          const file = processFileContent(
            rolePath,
            await readSshFile(sessionId, rolePath)
          );
          if (!file.isBinary && !file.isImage) {
            content = file.content.trim() || null;
          }
        } catch {
          content = null;
        }

        let includeGlobalRules = true;
        try {
          const settingsPath = `${projectRoot}/.snow/settings.json`;
          const settingsFile = processFileContent(
            settingsPath,
            await readSshFile(sessionId, settingsPath)
          );
          if (!settingsFile.isBinary && !settingsFile.isImage) {
            const settings = JSON.parse(settingsFile.content) as {
              role?: { includeGlobalRules?: unknown };
            };
            if (typeof settings.role?.includeGlobalRules === "boolean") {
              includeGlobalRules = settings.role.includeGlobalRules;
            }
          }
        } catch {
          includeGlobalRules = true;
        }

        return { content, includeGlobalRules };
      }
    );
  } catch {
    return { content: null, includeGlobalRules: true };
  }
};

const buildRemoteMkdirCommand = (remotePath: string): string =>
  `mkdir -p -- ${shellQuote(remotePath)}`;

const buildRemoteStatCommand = (remotePath: string): string =>
  `if [ -e ${shellQuote(remotePath)} ]; then printf present; fi`;

const ensureString = (value: unknown, fieldName: string): string => {
  if (typeof value !== "string") {
    throw new Error(`${fieldName} must be a string`);
  }
  return value;
};

const ensureOptionalPositiveInteger = (value: unknown): number | undefined => {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error("Line range values must be finite numbers");
  }
  return Math.max(1, Math.floor(value));
};

const replaceContent = (
  content: string,
  searchContent: string,
  replacement: string,
  occurrence: number
): { content: string; matchedLineStart: number; matchedLineEnd: number } => {
  if (occurrence < 1) {
    throw new Error("occurrence must be greater than zero");
  }

  let offset = 0;
  let foundIndex = -1;
  for (let index = 0; index < occurrence; index += 1) {
    foundIndex = content.indexOf(searchContent, offset);
    if (foundIndex < 0) {
      throw new Error("searchContent not found in remote file");
    }
    offset = foundIndex + Math.max(1, searchContent.length);
  }

  const prefix = content.slice(0, foundIndex);
  const matchedLineStart = prefix.split("\n").length;
  const matchedLineEnd =
    matchedLineStart + searchContent.split("\n").length - 1;
  return {
    content: `${prefix}${replacement}${content.slice(
      foundIndex + searchContent.length
    )}`,
    matchedLineStart,
    matchedLineEnd,
  };
};

const shellGlobExpression = (fileGlob: string | undefined): string => {
  if (!fileGlob) {
    return "*";
  }
  return fileGlob;
};

const buildRemoteGrepCommand = (
  remotePath: string,
  pattern: string,
  fileGlob: string | undefined,
  isRegex: boolean,
  caseSensitive: boolean,
  maxResults: number
): string => {
  const flags = ["-nH"];
  if (!isRegex) {
    flags.push("-F");
  }
  if (!caseSensitive) {
    flags.push("-i");
  }
  const glob = shellGlobExpression(fileGlob);
  // Normalize the remote path so a trailing slash cannot turn the find
  // `-path` pattern into a double-slash glob (e.g. `/src//*.tsx`) that
  // matches nothing. grep then receives no file arguments and falls back to
  // reading the never-ending SSH exec channel stdin until the 30s timeout.
  const root = normalizeRemotePath(remotePath);
  const script = [
    `root=${shellQuote(root)}`,
    `pattern=${shellQuote(pattern)}`,
    `glob=${shellQuote(glob)}`,
    `limit=${Math.max(1, maxResults)}`,
    // Build the find `-path` pattern so `root=/` stays a single slash.
    `if [ "$root" = "/" ]; then pathpat="/$glob"; else pathpat="$root/$glob"; fi`,
    // `-exec grep ... {} +` never runs grep without file arguments (unlike
    // `$(find ...)` command substitution), so a zero-match glob returns
    // immediately instead of blocking on stdin. Excluded directories move
    // from grep (where they never applied, since grep only receives file
    // arguments) to the find `-prune` stage. `< /dev/null` guards grep's
    // stdin as a last resort, and `2>/dev/null` silences find/grep noise
    // (also inherited by grep via fork). `head` still truncates the output
    // and `|| true` absorbs the resulting SIGPIPE exit code.
    `find "$root" \\( -type d -name .git -o -type d -name node_modules -o -type d -name target \\) -prune -o -type f -path "$pathpat" -exec grep ${flags
      .map(shellQuote)
      .join(" ")} -- "$pattern" {} + < /dev/null 2>/dev/null | head -n "$limit" || true`,
  ].join("\n");

  return `sh -lc ${shellQuote(script)}`;
};

const parseGrepLines = (
  output: string,
  workspacePath: string,
  remoteRootPath: string
): RemoteWorkspaceSearchMatch[] =>
  output.split("\n").flatMap((line) => {
    // Parse from the LEFT: `path:line:content` with the FIRST `:<digits>:`
    // pair as the separator. Content may contain colons (e.g. `case "x": y`),
    // so splitting from the last two colons would misparse the line number
    // and silently drop the match. File paths with embedded colons are
    // extremely rare on POSIX, and the lazy quantifier still skips them when
    // a `:<digits>:` separator exists later in the line.
    const parsed = /^(.+?):(\d+):(.*)$/.exec(line);
    if (!parsed) {
      return [];
    }
    const lineNumber = Number(parsed[2]);
    if (!Number.isInteger(lineNumber)) {
      return [];
    }
    return [
      {
        file: buildRemoteWorkspaceUri(workspacePath, parsed[1], remoteRootPath),
        line: lineNumber,
        content: parsed[3],
      },
    ];
  });

const executeFilesystemRead = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.filePath, "filePath");
  const startLine = ensureOptionalPositiveInteger(args.startLine);
  const endLine = ensureOptionalPositiveInteger(args.endLine);

  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      try {
        const entries = await listSshDirectory(sessionId, remotePath, {
          signal,
        });
        return {
          content: entries
            .map((entry) => `${entry.name}${entry.isDirectory ? "/" : ""}`)
            .join("\n"),
        };
      } catch (error) {
        if (isSshOperationError(error)) {
          throw error;
        }
        return readTextFile(workspacePath, startLine, endLine, signal);
      }
    },
    { signal }
  );
};

const executeFilesystemReplaceEdit = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.filePath, "filePath");
  const workspaceRoot = resolveAuthorizedWorkspaceRoot(
    workspacePath,
    args.workspaceRoot
  );
  const searchContent = ensureString(args.searchContent, "searchContent");
  const replacement = ensureString(args.replaceContent, "replaceContent");
  const occurrence =
    typeof args.occurrence === "number" && Number.isFinite(args.occurrence)
      ? Math.floor(args.occurrence)
      : 1;
  const loaded = await readRemoteText(workspacePath, signal);
  const result = replaceContent(
    loaded.content,
    searchContent,
    replacement,
    occurrence
  );
  const save = await writeRemoteText(
    workspacePath,
    workspaceRoot,
    result.content,
    loaded.version,
    signal
  );

  return {
    success: true,
    occurrence,
    matchType: "exact",
    matchedLineStart: result.matchedLineStart,
    matchedLineEnd: result.matchedLineEnd,
    saveGuarantee: save.guarantee,
    sideEffect: save.sideEffect,
  };
};

const executeFilesystemCreate = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.filePath, "filePath");
  const workspaceRoot = resolveAuthorizedWorkspaceRoot(
    workspacePath,
    args.workspaceRoot
  );
  const content = ensureString(args.content, "content");
  const overwrite = args.overwrite === true;

  const save = await withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const exists = (
        await executeSshCommand(sessionId, buildRemoteStatCommand(remotePath), {
          signal,
        })
      ).trim();
      if (exists && !overwrite) {
        throw new Error(
          "Remote file already exists. To overwrite this file, set overwrite=true."
        );
      }
      const parentPath = dirname(remotePath);
      if (parentPath && parentPath !== ".") {
        await executeSshCommand(
          sessionId,
          buildRemoteMkdirCommand(parentPath),
          {
            signal,
          }
        );
      }
      const expectedVersion: SshFileVersion = exists
        ? (await readSshFileWithVersion(sessionId, remotePath, { signal }))
            .version
        : { exists: false };
      return writeSshFile(sessionId, remotePath, content, {
        signal,
        workspaceRoot,
        expectedVersion,
      });
    },
    { signal }
  );

  return {
    success: true,
    path: workspacePath,
    bytes: Buffer.byteLength(content, "utf8"),
    lines: content.split("\n").length,
    saveGuarantee: save.guarantee,
    sideEffect: save.sideEffect,
  };
};

const executeGrepSearch = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  const pattern = ensureString(args.pattern, "pattern");
  const fileGlob =
    typeof args.fileGlob === "string" && args.fileGlob.trim()
      ? args.fileGlob.trim()
      : undefined;
  const isRegex = args.isRegex !== false;
  const caseSensitive = args.caseSensitive !== false;
  const maxResults =
    typeof args.maxResults === "number" && Number.isFinite(args.maxResults)
      ? Math.max(1, Math.floor(args.maxResults))
      : 100;

  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const output = await executeSshCommand(
        sessionId,
        buildRemoteGrepCommand(
          remotePath,
          pattern,
          fileGlob,
          isRegex,
          caseSensitive,
          maxResults
        ),
        { timeoutMs: REMOTE_GREP_TIMEOUT_MS, signal }
      );
      const matches = parseGrepLines(output, workspacePath, remotePath);
      return {
        backend: "remote-grep",
        pattern,
        path: workspacePath,
        fileGlob,
        matches,
        totalMatches: matches.length,
        truncated: matches.length >= maxResults,
        rawOutput: output.slice(0, 50_000),
      };
    },
    { signal }
  );
};

const executeBashCommand = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(
    args.workingDirectory,
    "workingDirectory"
  );
  const command = ensureString(args.command, "command");
  const timeout =
    typeof args.timeout === "number" && Number.isFinite(args.timeout)
      ? Math.max(1, Math.floor(args.timeout))
      : 30_000;

  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const wrappedCommand = `cd -- ${shellQuote(remotePath)} && ${command}`;
      // The timeout lives inside executeSshCommand so a timed-out command also
      // closes the exec channel and signals the remote process instead of
      // merely racing the promise and leaking the underlying process.
      const output = await executeSshCommand(sessionId, wrappedCommand, {
        timeoutMs: timeout,
        signal,
      });

      return {
        stdout: output,
        stderr: "",
        exitCode: 0,
        command,
        executedAt: new Date().toISOString(),
      };
    },
    { signal }
  );
};

// Mirrors SKIP_DIRS in native/src/storage/services/checkpoint/mod.rs so remote
// checkpoint scans skip the same heavy directories as local scans.
const CHECKPOINT_SKIP_DIRS = new Set([
  "node_modules",
  ".git",
  ".svn",
  ".hg",
  "dist",
  "build",
  ".next",
  ".nuxt",
  "out",
  "coverage",
  ".cache",
  ".turbo",
  ".vercel",
  "target",
  "__pycache__",
  ".venv",
  "venv",
  ".idea",
  ".vscode",
  ".vs",
  ".snow",
  ".snowapp",
  "release",
  ".output",
  ".angular",
  ".parcel-cache",
]);

const executeCheckpointStat = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const stats = await statSshEntry(sessionId, remotePath);
      if (!stats) {
        return { exists: false, isDirectory: false, size: 0, mtimeMs: 0 };
      }
      return {
        exists: true,
        isDirectory: stats.isDirectory(),
        size: stats.size,
        mtimeMs: stats.mtime * 1000,
      };
    },
    { signal }
  );
};

const executeCheckpointListTree = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const entries: Array<{
        path: string;
        isDirectory: boolean;
        size: number;
        mtimeMs: number;
      }> = [];
      // 每个目录的 .gitignore 内容（dir 为相对根目录的 POSIX 路径，
      // 根目录为 ""）。Rust 侧复用与本地相同的 GitignoreMatcher 语义。
      const gitignores: Array<{ dir: string; content: string }> = [];
      const directories: Array<{ absolute: string; relative: string }> = [
        { absolute: remotePath, relative: "" },
      ];
      while (directories.length > 0) {
        const { absolute, relative } = directories.pop() as {
          absolute: string;
          relative: string;
        };
        const list = await listSshDirectory(sessionId, absolute, { signal });
        for (const entry of list) {
          // Symlinks are never captured (local scans skip them too).
          if (entry.isSymbolicLink) {
            continue;
          }
          const entryRelative = relative
            ? `${relative}/${entry.name}`
            : entry.name;
          if (entry.isDirectory) {
            if (!CHECKPOINT_SKIP_DIRS.has(entry.name)) {
              directories.push({ absolute: entry.path, relative: entryRelative });
            }
            continue;
          }
          if (entry.name === ".gitignore") {
            // 收集规则内容供 Rust 侧过滤；读取失败只丢规则不中断扫描。
            try {
              const buf = await readSshFile(sessionId, entry.path, { signal });
              gitignores.push({
                dir: relative,
                content: buf.toString("utf-8"),
              });
            } catch {
              // Best effort — the file tree scan continues without these rules.
            }
          }
          entries.push({
            path: entryRelative,
            isDirectory: false,
            size: entry.size,
            mtimeMs: entry.mtime * 1000,
          });
        }
      }
      entries.sort((a, b) => a.path.localeCompare(b.path));
      return { entries, gitignores };
    },
    { signal }
  );
};

const executeCheckpointReadFile = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      let content: Buffer;
      try {
        content = await readSshFile(sessionId, remotePath, { signal });
      } catch (error) {
        if (isSshOperationError(error)) {
          throw error;
        }
        // The file may have been removed between stat and read.
        return { content: null };
      }
      return { content: content.toString("base64") };
    },
    { signal }
  );
};

const executeCheckpointWriteFile = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  const contentBase64 = ensureString(args.contentBase64, "contentBase64");
  const data = Buffer.from(contentBase64, "base64");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const parentPath = dirname(remotePath);
      if (parentPath && parentPath !== ".") {
        await executeSshCommand(
          sessionId,
          buildRemoteMkdirCommand(parentPath),
          { signal }
        );
      }
      const save = await writeInternalSshFile(sessionId, remotePath, data, {
        signal,
      });
      return { bytes: save.bytes };
    },
    { signal }
  );
};

const executeCheckpointDeleteFile = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      try {
        await deleteSshFile(sessionId, remotePath);
        return { deleted: true };
      } catch {
        return { deleted: false };
      }
    },
    { signal }
  );
};

const executeCheckpointRemoveDir = async (
  args: RemoteWorkspaceCommandArgs,
  signal?: AbortSignal
): Promise<Record<string, unknown>> => {
  const workspacePath = validateSshWorkspacePath(args.path, "path");
  return withSshSession(
    workspacePath,
    async (sessionId, remotePath) => {
      const removed = await removeEmptySshDirectory(sessionId, remotePath);
      return { removed };
    },
    { signal }
  );
};

export const dispatchRemoteWorkspaceCommand = async (
  command: RemoteWorkspaceCommand,
  options?: { signal?: AbortSignal }
): Promise<string> => {
  const signal = options?.signal;
  let args: RemoteWorkspaceCommandArgs;
  try {
    args = JSON.parse(command.argsJson) as RemoteWorkspaceCommandArgs;
  } catch {
    throw new Error("Remote workspace command arguments must be valid JSON");
  }

  try {
    let result: Record<string, unknown>;
    switch (command.operation) {
      case "filesystem-read":
        result = await executeFilesystemRead(args, signal);
        break;
      case "filesystem-replace_edit":
        result = await executeFilesystemReplaceEdit(args, signal);
        break;
      case "filesystem-create":
        result = await executeFilesystemCreate(args, signal);
        break;
      case "grep-search":
        result = await executeGrepSearch(args, signal);
        break;
      case "bash-terminal-execute":
        result = await executeBashCommand(args, signal);
        break;
      case "checkpoint-stat":
        result = await executeCheckpointStat(args, signal);
        break;
      case "checkpoint-list-tree":
        result = await executeCheckpointListTree(args, signal);
        break;
      case "checkpoint-read-file":
        result = await executeCheckpointReadFile(args, signal);
        break;
      case "checkpoint-write-file":
        result = await executeCheckpointWriteFile(args, signal);
        break;
      case "checkpoint-delete-file":
        result = await executeCheckpointDeleteFile(args, signal);
        break;
      case "checkpoint-remove-dir":
        result = await executeCheckpointRemoveDir(args, signal);
        break;
      default:
        throw new Error(
          `Unsupported remote workspace operation: ${command.operation}`
        );
    }
    return JSON.stringify(result);
  } catch (error) {
    if (isSshOperationError(error)) {
      return JSON.stringify({
        success: false,
        error: toSshOperationErrorResult(error),
      });
    }
    throw error;
  }
};
