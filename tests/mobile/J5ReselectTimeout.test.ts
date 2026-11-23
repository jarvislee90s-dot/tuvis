import { describe, expect, it } from "vitest";
import { OPENCODE_RESELECT_POST_TIMEOUT_MS } from "../../src/mobile/opencodeNativeApi";

describe("重选客户端等待", () => {
  it("按 50 秒结束等待并留给回执查询处理", () => {
    expect(OPENCODE_RESELECT_POST_TIMEOUT_MS).toBe(50_000);
  });
});
