// 错误码门禁的源码视图：注释、字面量与 cfg(test) 共用词法边界，避免各规则互相遮蔽。
const blank = (s) => s.replace(/[^\r\n]/g, " ");

export function sourceView(src) {
  const comments = [];
  const literals = [];
  for (let i = 0; i < src.length;) {
    const start = i;
    if (src.startsWith("//", i)) {
      i = src.indexOf("\n", i);
      if (i < 0) i = src.length;
      comments.push([start, i]);
    } else if (src.startsWith("/*", i)) {
      i += 2;
      let depth = 1;
      while (i < src.length && depth) {
        if (src.startsWith("/*", i)) {
          depth++;
          i += 2;
        } else if (src.startsWith("*/", i)) {
          depth--;
          i += 2;
        } else i++;
      }
      comments.push([start, i]);
    } else {
      // Rust 原始串支持任意 # 数，含 byte/C 变体；不能在标识符中间认开串。
      const raw = !/[\w]/.test(src[i - 1] ?? "") ? /^(?:br|cr|r)(#*)"/.exec(src.slice(i)) : null;
      if (raw) {
        const contentStart = i + raw[0].length;
        const close = src.indexOf('"' + raw[1], contentStart);
        i = close < 0 ? src.length : close + 1 + raw[1].length;
        literals.push({ start, end: i, text: src.slice(contentStart, close < 0 ? i : close) });
      } else if (src[i] === '"') {
        const quote = src[i++];
        const contentStart = i;
        while (i < src.length && src[i] !== quote) {
          if (src[i] === "\\") i++;
          i++;
        }
        literals.push({
          start,
          end: Math.min(src.length, i + 1),
          text: src.slice(contentStart, i),
        });
        i = Math.min(src.length, i + 1);
      } else if (src[i] === "'") {
        // 只认带闭引号的单字符/转义字符；'a / 'static / 标签不是字面量。
        const ch = /^'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\r\n])|[^'\\\r\n])'/u.exec(
          src.slice(i)
        );
        if (ch) {
          i += ch[0].length;
          literals.push({ start, end: i, text: "" });
        } else i++;
      } else i++;
    }
  }
  const mask = (ranges) => {
    let out = "",
      end = 0;
    for (const [lo, hi] of ranges.sort((a, b) => a[0] - b[0])) {
      out += src.slice(end, lo) + blank(src.slice(lo, hi));
      end = hi;
    }
    return out + src.slice(end);
  };
  return {
    text: mask([...comments]),
    skeleton: mask([...comments, ...literals.map((l) => [l.start, l.end])]),
    literals,
  };
}

let ts;
export async function frontendView(src, file) {
  // 使用项目已有编译器，避免正则里的引号、JSX、嵌套模板把手写词法器带偏。
  ts ??= (await import("typescript")).default;
  const tree = ts.createSourceFile(file, src, ts.ScriptTarget.Latest, true);
  const literals = [];
  function visit(node) {
    if (ts.isStringLiteralLike(node) || ts.isTemplateExpression(node)) {
      const start = node.getStart(tree);
      const end = node.getEnd();
      literals.push({ start, end, text: src.slice(start + 1, end - 1) });
    }
    ts.forEachChild(node, visit);
  }
  visit(tree);
  return { text: src, skeleton: src, literals };
}

// test=false 时 cfg 是否仍可能成立；未知的平台/feature 谓词取「可能」。
// 仅当必定不成立才删，保证 not(test)、any(windows,test) 等生产面留在扫描内。
function cfgWithoutTest(expr) {
  const tokens = expr.match(/[A-Za-z_][\w]*|[(),]|[^\s(),]+/g) ?? [];
  let i = 0;
  function parse() {
    const name = tokens[i++];
    if (tokens[i] !== "(") {
      if (tokens[i] === "=") {
        i++;
        while (i < tokens.length && tokens[i] !== "," && tokens[i] !== ")") i++;
        return [false, true];
      }
      return name === "test" ? [false] : [false, true];
    }
    i++;
    const args = [];
    while (i < tokens.length && tokens[i] !== ")") {
      args.push(parse());
      if (tokens[i] === ",") i++;
      else break;
    }
    if (tokens[i] !== ")") return [false, true];
    i++;
    if (name === "not" && args.length === 1) return args[0].map((v) => !v);
    if (name === "all")
      return [
        args.every((a) => a.includes(true)),
        ...(args.some((a) => a.includes(false)) ? [false] : []),
      ];
    if (name === "any")
      return [
        args.some((a) => a.includes(true)),
        ...(args.every((a) => a.includes(false)) ? [false] : []),
      ];
    return [false, true];
  }
  const values = parse();
  return i === tokens.length ? values.includes(true) : true;
}

export function matchingClose(skeleton, open) {
  const pairs = { "(": ")", "[": "]", "{": "}" };
  const stack = [pairs[skeleton[open]]];
  for (let i = open + 1; i < skeleton.length; i++) {
    const c = skeleton[i];
    if (pairs[c]) stack.push(pairs[c]);
    else if (c === stack.at(-1)) {
      stack.pop();
      if (!stack.length) return i;
    }
  }
  return skeleton.length - 1;
}

/**
 * `<…>` 配对（尖括号不在 matchingClose 的括号族里）。
 * 找不到配对返回 -1 —— 调用侧据此退回「当成比较运算符」的保守处理。
 */
export function matchingAngle(skeleton, open) {
  let depth = 0;
  for (let i = open; i < skeleton.length; i++) {
    const c = skeleton[i];
    if (c === "<") depth++;
    else if (c === ">") {
      const prev = skeleton[i - 1];
      if (prev === "-" || prev === "=") continue; // `->` / `=>` / `>=` 的 `>` 不是收口
      depth--;
      if (!depth) return i;
    } else if (c === ";" || c === "{" || c === "}") return -1; // 类型实参位不会出现这些
  }
  return -1;
}

export function stripTests(src) {
  const { skeleton } = sourceView(src);
  const cuts = [];
  const attrs = /#\s*\[\s*cfg\s*\(/g;
  for (const m of skeleton.matchAll(attrs)) {
    const open = m.index + m[0].length - 1;
    const close = matchingClose(skeleton, open);
    if (cfgWithoutTest(skeleton.slice(open + 1, close))) continue;
    let j = skeleton.indexOf("]", close) + 1;
    if (j === 0) continue;
    // cfg 后可能叠有其它属性；只在真正的 item 起点判定分隔符。
    while (j < skeleton.length) {
      if (/\s/.test(skeleton[j])) j++;
      else if (skeleton[j] === "#" && skeleton[j + 1] === "[")
        j = matchingClose(skeleton, j + 1) + 1;
      else break;
    }
    const start = j;
    let block = false;
    let arm = false;
    while (j < skeleton.length) {
      const c = skeleton[j];
      if (c === "(" || c === "[") {
        j = matchingClose(skeleton, j) + 1;
        continue;
      }
      // 泛型/类型实参位（如 `-> Result<(String, u32), String>`）：型参里的顶层逗号
      // 不是条目终止符。判定要求「前一个非空字符像标识符/收口括号」且「后一个像类型」，
      // 且必须有配对 `>`，否则退回按普通字符处理（比较运算符 `a < b` 的保守路径）。
      if (c === "<") {
        const prev = skeleton.slice(0, j).trimEnd().slice(-1);
        const next = skeleton.slice(j + 1).trimStart()[0] ?? "";
        if (/[A-Za-z0-9_>)\]]/.test(prev) && /[A-Za-z_&'[(]/.test(next)) {
          const gt = matchingAngle(skeleton, j);
          if (gt > j) {
            j = gt + 1;
            continue;
          }
        }
        j++;
        continue;
      }
      if (c === "{") {
        j = matchingClose(skeleton, j) + 1;
        const tail = skeleton.slice(j).trimStart();
        // struct pattern 后还有 =>（可带 guard）；RHS 的 if/else 则要跨过全部分支。
        if (/^(?:=>|if\b|else\b)/.test(tail)) continue;
        // arm 内嵌结构的后缀（字段访问/?/运算）属于同一表达式。
        if (arm && /^(?:[.?+*/&|]|-(?!>)|==|!=)/.test(tail)) continue;
        block = true;
        break;
      }
      if (skeleton.startsWith("=>", j)) {
        arm = true;
        j += 2;
        continue;
      }
      if (c === ";" || c === ",") {
        j++;
        break;
      }
      if (c === "}") break; // 最后一条 enum 变体 / match arm，无尾逗号
      j++;
    }
    if (j > start || block) cuts.push([m.index, j]);
  }
  // 用空白替换，保留行号和偏移；嵌套 cfg 区间合并后只切一次。
  let out = "",
    end = 0;
  for (const [lo, hi] of cuts) {
    if (hi <= end) continue;
    out += src.slice(end, Math.max(end, lo)) + blank(src.slice(Math.max(end, lo), hi));
    end = hi;
  }
  return out + src.slice(end);
}

// 只豁免调用自身的实参，不让 ±N 行窗口里的日志/结构化码族遮住邻近上屏错误。
export function callRanges(skeleton, pattern) {
  return [...skeleton.matchAll(pattern)].map((m) => {
    const open = m.index + m[0].lastIndexOf("(");
    return [open, matchingClose(skeleton, open)];
  });
}
