"""按 Snow App v0.5.16 官方「分层增量压缩」算法，预测一次折叠的 token 账。

用途：不安装官方包，也能对任意已有会话算出官方口径的
removed / retained / 压缩后上下文 / 压缩率。

算法逐行对照 upstream:
  native/src/api/conversation/context_fold.rs      (plan_context_fold / resolve_merge_start / build_instruction)
  native/src/storage/services/chat_conversations/context_folds.rs (ContextFoldMeta / build_view / item_tokens)
  native/src/storage/services/chat_conversations/mod.rs            (load_context_view 的 SQL)
token 计数用与官方相同的 o200k_base（tiktoken），因此 removed/retained 是精确值；
summary 的 token 数只能预估（官方由模型产出），用 --summary-tokens 指定。

依赖：pip install tiktoken
用法：
  python scripts/measure-official-fold.py --db ~/.snowapp/snowapp.db --list
  python scripts/measure-official-fold.py --db ~/.snowapp/snowapp.db --conversation conv-xxx
  python scripts/measure-official-fold.py --db ~/.snowapp/snowapp.db --top 5
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3

import tiktoken

TAIL_KEEP_PERCENT = 30
SUMMARY_SHARE_MERGE_PERCENT = 40
MERGE_FANOUT = 4
MAX_TIER = 3

ENCODER = tiktoken.get_encoding("o200k_base")


def count_tokens(text: str | None) -> int:
    if not text:
        return 0
    return len(ENCODER.encode(text, disallowed_special=()))


ROW_SQL = """
SELECT role, content, tool_calls_json, raw_json, thinking, thinking_blocks_json, origin, id, status
  FROM chat_messages
 WHERE conversation_id = ?
   AND id >= COALESCE(
     (SELECT id
        FROM chat_messages
       WHERE conversation_id = ?
         AND status = 'context_compaction'
         AND instr(raw_json, '"contextFold"') = 0
       ORDER BY id DESC
       LIMIT 1),
     '')
   AND (
     content <> ''
     OR (role = 'assistant' AND tool_calls_json <> '' AND tool_calls_json <> '[]')
     OR (role = 'assistant' AND thinking <> '')
   )
   AND NOT (role = 'assistant' AND status = 'error')
   AND NOT (
     role = 'user'
     AND NOT (status = 'context_compaction' AND instr(raw_json, '"contextFold"') > 0)
     AND EXISTS (
       SELECT 1 FROM chat_messages nxt
        WHERE nxt.conversation_id = chat_messages.conversation_id
          AND nxt.id > chat_messages.id
          AND nxt.id = (
            SELECT MIN(id) FROM chat_messages
             WHERE conversation_id = chat_messages.conversation_id
               AND id > chat_messages.id
          )
          AND nxt.role = 'assistant'
          AND nxt.status = 'error'
     )
   )
 ORDER BY id ASC
"""


def load_rows(con: sqlite3.Connection, conversation_id: str) -> list[dict]:
    rows = []
    for (
        role,
        content,
        tool_calls_raw,
        raw_json,
        thinking_raw,
        thinking_blocks_raw,
        origin,
        row_id,
        status,
    ) in con.execute(ROW_SQL, (conversation_id, conversation_id)):
        fold = None
        if status == "context_compaction" and '"contextFold"' in raw_json:
            try:
                fold = json.loads(raw_json)["contextFold"]
            except (ValueError, KeyError, TypeError):
                fold = None
        tool_calls_json = (
            tool_calls_raw
            if tool_calls_raw and tool_calls_raw != "[]"
            else None
        )
        tool_results_json = (
            raw_json
            if role.strip() == "tool" and raw_json and raw_json != "{}"
            else None
        )
        thinking = thinking_raw or None
        thinking_blocks_json = (
            thinking_blocks_raw
            if role.strip() == "assistant"
            and thinking_blocks_raw
            and thinking_blocks_raw != "[]"
            else None
        )
        rows.append(
            {
                "id": row_id,
                "role": role,
                "content": content,
                "fold": fold,
                "tool_calls_json": tool_calls_json,
                "tool_results_json": tool_results_json,
                "thinking": thinking,
                "thinking_blocks_json": thinking_blocks_json,
            }
        )
    return rows


def item_tokens(item: dict) -> int:
    if item["kind"] == "fold":
        return count_tokens(item["content"])
    return (
        count_tokens(item["content"])
        + count_tokens(item["tool_calls_json"])
        + count_tokens(item["tool_results_json"])
        + count_tokens(item["thinking"])
    )


def build_view(rows: list[dict]) -> list[dict]:
    """官方 build_view：被更新的折叠完整包含的旧折叠视为已合并。"""
    folds = [(row["fold"], row["content"]) for row in rows if row["fold"]]
    messages = [(row["id"], row) for row in rows if not row["fold"]]
    if not folds:
        return [{"kind": "message", **row} for _, row in messages]

    active = []
    for index, (meta, summary) in enumerate(folds):
        contained = any(
            later["startId"] <= meta["startId"] and meta["endId"] <= later["endId"]
            for later, _ in folds[index + 1 :]
        )
        if not contained:
            active.append((meta, summary))
    active.sort(key=lambda pair: pair[0]["startId"])

    view: list[dict] = []
    pending = list(active)
    current = None
    for row_id, row in messages:
        while pending and pending[0][0]["startId"] <= row_id:
            meta, summary = pending.pop(0)
            current = meta
            view.append(
                {
                    "kind": "fold",
                    "id": meta["startId"],
                    "content": f"[Compressed conversation section #{meta['seq']} · tier {meta['tier']}]\n{summary.strip()}",
                    "meta": meta,
                }
            )
        if current and current["startId"] <= row_id <= current["endId"]:
            continue
        view.append({"kind": "message", **row})
    for meta, summary in pending:
        view.append(
            {
                "kind": "fold",
                "id": meta["startId"],
                "content": f"[Compressed conversation section #{meta['seq']} · tier {meta['tier']}]\n{summary.strip()}",
                "meta": meta,
            }
        )
    return view


def is_tool_result(item: dict) -> bool:
    return item["kind"] == "message" and item["role"].strip() == "tool"


def resolve_merge_start(folds, raw_start, summary_tokens, total_tokens):
    if folds and summary_tokens * 100 > total_tokens * SUMMARY_SHARE_MERGE_PERCENT:
        top = max(tier for _, tier in folds)
        return 0, min(top + 1, MAX_TIER)
    start = raw_start
    tier = 1
    cursor = len(folds)
    while True:
        begin = cursor
        while begin > 0 and folds[begin - 1][1] == tier:
            begin -= 1
        if cursor - begin + 1 < MERGE_FANOUT:
            break
        start = folds[begin][0]
        cursor = begin
        if tier >= MAX_TIER:
            break
        tier += 1
    return start, tier


def build_instruction(kept, merged, tier):
    if kept:
        first, last = kept[0], kept[-1]
        if first == last:
            rng = (
                f"Compressed section #{last} at the beginning is already summarized; keep it out of "
                f"your summary. Compress everything AFTER section #{last} up to the end of the "
                f"conversation above into ONE summary."
            )
        else:
            rng = (
                f"Compressed sections #{first}-#{last} at the beginning are already summarized; keep "
                f"them out of your summary. Compress everything AFTER section #{last} up to the end "
                f"of the conversation above into ONE summary."
            )
    else:
        rng = "Compress the ENTIRE conversation above into ONE summary."
    merge = ""
    if merged:
        merge = (
            f" That range begins with earlier compressed sections #{merged[0]}-#{merged[-1]}: they "
            f"are summaries themselves, so merge them with the newer messages into one consolidated "
            f"summary instead of repeating them."
        )
    detail = (
        "Be detailed: keep identifiers, file paths, symbols, commands, numbers and error messages "
        "exactly. Stay within roughly 1500 words."
        if tier <= 1
        else "Consolidate: drop details that no longer matter for future work, but keep every user "
        "request, decision, constraint and file path. Stay within roughly 1200 words."
    )
    return (
        "[Context compression request]\n"
        f"The context window is filling up. {rng}{merge} Your summary replaces those messages in "
        "every future request (the originals stay stored and remain searchable), so it must let you "
        "continue the work without re-reading them.\n\n"
        "Write the summary in Markdown with these sections:\n"
        "## User Requests\n"
        "Every user message in the range, quoted verbatim in its original language (abbreviate only "
        "long pasted content, keeping its purpose).\n"
        "## Work Done\n"
        "Concrete actions and outcomes with exact file paths, symbols, commands and results. Judge "
        "completion from evidence (applied edits, tool output), not from TODO status.\n"
        "## Findings & Decisions\n"
        "Facts learned, decisions made, constraints, errors and how they were resolved.\n"
        "## Open Threads\n"
        "What was still in progress or pending at the end of the range, with the exact next step.\n\n"
        "Rules:\n"
        f"- {detail}\n"
        "- Output ONLY the summary. Do not call tools, do not address the user, and do not add a "
        "title line."
    )


def plan(con, conversation_id, summary_tokens):
    view = build_view(load_rows(con, conversation_id))
    if not view:
        return None
    tokens = [item_tokens(item) for item in view]
    total = sum(tokens)
    length = len(view)
    raw_start = 0
    for index, item in enumerate(view):
        if item["kind"] == "fold":
            raw_start = index + 1

    keep_budget = total * TAIL_KEEP_PERCENT // 100
    split = length
    kept_tokens = 0
    while split > raw_start and kept_tokens + tokens[split - 1] <= keep_budget:
        kept_tokens += tokens[split - 1]
        split -= 1
    while split > raw_start and split < length and is_tool_result(view[split]):
        split -= 1
    if split <= raw_start:
        split = length

    folds = [(index, item["meta"]["tier"]) for index, item in enumerate(view[:raw_start]) if item["kind"] == "fold"]
    summary_tokens_in_view = sum(tokens[:raw_start])
    start, tier = resolve_merge_start(folds, raw_start, summary_tokens_in_view, total)

    rng = view[start:split]
    raw_in_range = [item for item in rng if item["kind"] == "message"]
    folds_in_range = [item for item in rng if item["kind"] == "fold"]
    if not raw_in_range and len(folds_in_range) < 2:
        return {
            "conversationId": conversation_id,
            "totalTokens": total,
            "viewItems": length,
            "error": "Nothing to compress yet: the context has no older messages that can be folded",
        }

    kept = [item["meta"]["seq"] for item in view[:start] if item["kind"] == "fold"]
    merged = [item["meta"]["seq"] for item in folds_in_range]
    instruction = build_instruction(kept, merged, tier)
    instruction_tokens = count_tokens(instruction)
    removed_raw = sum(tokens[start:split])
    removed = removed_raw + instruction_tokens
    retained = sum(tokens[split:])
    post = retained + summary_tokens
    return {
        "conversationId": conversation_id,
        "totalTokens": total,
        "viewItems": length,
        "tier": tier,
        "foldStart": start,
        "foldEnd": split - 1,
        "foldMessages": len(raw_in_range),
        "foldTokens": removed_raw,
        "foldedSections": merged,
        "keptSections": kept,
        "instructionTokens": instruction_tokens,
        "removedTokens": removed,
        "retainedTokens": retained,
        "retainedMessages": length - split,
        "summaryTokens": summary_tokens,
        "postFoldTokens": post,
        "ratio": round(1 - post / total, 4) if total else 0.0,
        "requestInputTokens": sum(tokens[:split]) + instruction_tokens,
    }


def report_folds(con, conversation_id):
    """真实跑过官方折叠后，从库里回算每一段与累计的压缩率。"""
    rows = load_all_rows(con, conversation_id)
    raw_rows = [row for row in rows if not row["fold"]]
    folds = [(row["fold"], row["content"]) for row in rows if row["fold"]]
    if not folds:
        return {
            "conversationId": conversation_id,
            "folds": 0,
            "error": "no contextFold rows; the official incremental compression never ran here",
        }
    raw_total = sum(item_tokens({"kind": "message", **row}) for row in raw_rows)
    view = build_view(rows)
    post_total = sum(item_tokens(item) for item in view)

    active = []
    for index, (meta, summary) in enumerate(folds):
        contained = any(
            later["startId"] <= meta["startId"] and meta["endId"] <= later["endId"]
            for later, _ in folds[index + 1 :]
        )
        if not contained:
            active.append((meta, summary))

    details = []
    for meta, summary in active:
        covered = [
            row
            for row in raw_rows
            if meta["startId"] <= row["id"] <= meta["endId"]
        ]
        covered_tokens = sum(item_tokens({"kind": "message", **row}) for row in covered)
        summary_tokens = count_tokens(summary)
        details.append(
            {
                "section": meta["seq"],
                "tier": meta["tier"],
                "startId": meta["startId"],
                "endId": meta["endId"],
                "coveredMessages": len(covered),
                "coveredTokens": covered_tokens,
                "summaryTokens": summary_tokens,
                "savedTokens": covered_tokens - summary_tokens,
                "foldRatio": round(1 - summary_tokens / covered_tokens, 4)
                if covered_tokens
                else 0.0,
            }
        )
    return {
        "conversationId": conversation_id,
        "folds": len(folds),
        "activeSections": len(active),
        "rawTokens": raw_total,
        "contextTokens": post_total,
        "savedTokens": raw_total - post_total,
        "ratio": round(1 - post_total / raw_total, 4) if raw_total else 0.0,
        "sections": details,
    }


def load_all_rows(con: sqlite3.Connection, conversation_id: str) -> list[dict]:
    rows = []
    for (
        role,
        content,
        tool_calls_raw,
        raw_json,
        thinking_raw,
        thinking_blocks_raw,
        origin,
        row_id,
        status,
    ) in con.execute(
        "SELECT role, content, tool_calls_json, raw_json, thinking, thinking_blocks_json, "
        "origin, id, status FROM chat_messages WHERE conversation_id = ? ORDER BY id ASC",
        (conversation_id,),
    ):
        if role == "assistant" and status == "error":
            continue
        fold = None
        if status == "context_compaction" and '"contextFold"' in raw_json:
            try:
                fold = json.loads(raw_json)["contextFold"]
            except (ValueError, KeyError, TypeError):
                fold = None
        if not fold and not content and not (
            role == "assistant" and tool_calls_raw and tool_calls_raw != "[]"
        ) and not (role == "assistant" and thinking_raw):
            continue
        rows.append(
            {
                "id": row_id,
                "role": role,
                "content": content,
                "fold": fold,
                "tool_calls_json": tool_calls_raw
                if tool_calls_raw and tool_calls_raw != "[]"
                else None,
                "tool_results_json": raw_json
                if role.strip() == "tool" and raw_json and raw_json != "{}"
                else None,
                "thinking": thinking_raw or None,
                "thinking_blocks_json": thinking_blocks_raw or None,
            }
        )
    return rows


def simulate(con, conversation_id, threshold, budget, summary_tokens):
    """对比三种策略在真实消息序列上的每请求上下文占用。

    每个消息视为一次请求边界（工具连续调用会略高估，方向不变）。
    official: 达到 threshold 才折叠，保留 30% 尾部 + 摘要。
    budget:   达到 budget 就折叠，其余同上（代表"预算驱动"的稳态）。
    none:     从不折叠，仅作基线。
    """
    rows = [row for row in load_all_rows(con, conversation_id) if not row["fold"]]
    tokens = [item_tokens({"kind": "message", **row}) for row in rows]
    if not tokens:
        return {"conversationId": conversation_id, "error": "no messages"}

    def run(cap, fold_request_costs):
        context: list[int] = []
        total = 0
        peak = 0
        folds = 0
        for token in tokens:
            context.append(token)
            size = sum(context)
            if size >= cap:
                keep_budget = size * TAIL_KEEP_PERCENT // 100
                kept = 0
                cut = len(context)
                while cut > 0 and kept + context[cut - 1] <= keep_budget:
                    kept += context[cut - 1]
                    cut -= 1
                context = [summary_tokens] + context[cut:]
                folds += 1
                if fold_request_costs:
                    # 折叠调用本身要发一次完整前缀（缓存冷启动）
                    total += size
            size = sum(context)
            peak = max(peak, size)
            total += size
        requests = len(tokens) + (folds if fold_request_costs else 0)
        new_tokens = sum(tokens)
        return {
            "cap": cap,
            "requests": requests,
            "folds": folds,
            "totalInputTokens": total,
            "avgInputPerRequest": round(total / requests) if requests else 0,
            "peakContextTokens": peak,
            "finalContextTokens": sum(context),
            # 信噪比：每读入 1 个新 token，要连带重读多少旧 token
            "newTokens": new_tokens,
            "staleRatio": round(1 - new_tokens / total, 4) if total else 0.0,
            "costPerNewToken": round(total / new_tokens, 2) if new_tokens else 0.0,
        }

    return {
        "conversationId": conversation_id,
        "messages": len(tokens),
        "rawTokens": sum(tokens),
        "officialThreshold": run(threshold, True),
        "budgetDriven": run(budget, True),
        "noCompression": run(10**12, False),
    }


def composition(con, conversation_id):
    """按角色/工具名统计 token 占比：确定哪些内容值得外置。"""
    rows = [row for row in load_all_rows(con, conversation_id) if not row["fold"]]
    by_role: dict[str, int] = {}
    by_tool: dict[str, int] = {}
    total = 0
    for row in rows:
        item = {"kind": "message", **row}
        tokens = item_tokens(item)
        total += tokens
        role = row["role"].strip() or "unknown"
        by_role[role] = by_role.get(role, 0) + tokens
        if role == "tool":
            name = "unknown"
            try:
                parsed = json.loads(row["tool_results_json"] or "[]")
                if parsed:
                    name = parsed[0].get("name") or "unknown"
            except (ValueError, TypeError, AttributeError):
                pass
            by_tool[name] = by_tool.get(name, 0) + tokens
    return {
        "conversationId": conversation_id,
        "totalTokens": total,
        "byRole": {
            role: {"tokens": tokens, "share": round(tokens / total, 4)}
            for role, tokens in sorted(by_role.items(), key=lambda kv: -kv[1])
        },
        "byTool": {
            name: {"tokens": tokens, "share": round(tokens / total, 4)}
            for name, tokens in sorted(by_tool.items(), key=lambda kv: -kv[1])[:15]
        },
        # 确定性外置的收益上限：单条工具结果超过阈值即可换成占位 + ref
        "externalizable": {
            str(threshold): {
                "tokens": sum(
                    item_tokens({"kind": "message", **row})
                    for row in rows
                    if row["role"].strip() == "tool"
                    and item_tokens({"kind": "message", **row}) > threshold
                ),
                "share": round(
                    sum(
                        item_tokens({"kind": "message", **row})
                        for row in rows
                        if row["role"].strip() == "tool"
                        and item_tokens({"kind": "message", **row}) > threshold
                    )
                    / total,
                    4,
                ),
            }
            for threshold in (1000, 2000, 4000, 8000)
        },
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", default=os.path.expanduser("~/.snowapp/snowapp.db"))
    parser.add_argument("--conversation")
    parser.add_argument("--top", type=int, default=0)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--folds", action="store_true", help="回算库里已有的官方折叠")
    parser.add_argument("--simulate", action="store_true", help="对比阈值触发与预算驱动")
    parser.add_argument("--composition", action="store_true", help="按角色/工具统计体积占比")
    parser.add_argument("--threshold", type=int, default=160000)
    parser.add_argument("--budget", type=int, default=40000)
    parser.add_argument("--summary-tokens", type=int, default=2000)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    con = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)

    if args.list:
        for row in con.execute(
            "SELECT conversation_id, COUNT(*) n, SUM(LENGTH(content)) chars "
            "FROM chat_messages GROUP BY conversation_id ORDER BY chars DESC LIMIT 20"
        ):
            print(row)
        return

    targets = []
    if args.conversation:
        targets = [args.conversation]
    elif args.top:
        targets = [
            row[0]
            for row in con.execute(
                "SELECT conversation_id FROM chat_messages GROUP BY conversation_id "
                "ORDER BY SUM(LENGTH(content)) DESC LIMIT ?",
                (args.top,),
            )
        ]
    else:
        parser.error("specify --conversation, --top N, or --list")

    def measure(cid):
        if args.folds:
            return report_folds(con, cid)
        if args.simulate:
            return simulate(con, cid, args.threshold, args.budget, args.summary_tokens)
        if args.composition:
            return composition(con, cid)
        return plan(con, cid, args.summary_tokens)

    results = [measure(cid) for cid in targets]
    if args.json:
        print(json.dumps(results, ensure_ascii=False, indent=2))
        return
    for result in results:
        print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
