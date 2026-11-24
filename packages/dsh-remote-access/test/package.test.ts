/**
 * T03 包级契约测试（RED 先行）：
 *
 * 1. packed_bundle_boots_and_disposes —— 构建产物 lib/index.js 装进真实 Cordis
 *    上下文，注入假 connection/webServer（记录路由注册），断言注册行为与
 *    dispose 后彻底清理（注册面清空）。
 * 2. archive_has_no_lifecycle_script —— tgz 内打包清单不得带任何生命周期脚本
 *    （安装版不需要用户系统 Node/网络构建的一部分）。
 * 3. host_react_is_not_bundled —— tgz 不得内置 React（宿主提供 React 18，仅 peer）。
 * 4. build_is_byte_reproducible —— 两次构建逐字节一致、与已提交 manifest 一致、
 *    归档不含绝对开发路径。
 *
 * 观测全部来自测试边界（假服务的记录、归档字节、构建函数返回值），
 * 不读 ~/.dsh，不连真实 DSH。
 */

import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it, onTestFinished } from 'vitest';
import { Context } from '@deepseek-ai/cordis';
import type { WebRoute } from '@deepseek-ai/dsh-host-webserver';
// 被测对象 = 构建产物入口（不是 src 源码）
import plugin, { apply as builtApply } from '../lib/index.js';
// 协议代际从源码常量读取：测试不得再写死字面量（曾因此漏检归档协议漂移）。
import { BINDING_PROTOCOL } from '../src/types.js';
import { buildReleaseArtifacts, buildPackedPackageManifest, listTarEntries } from '../../scripts/build-dsh-plugin.mjs';

const thisDir = dirname(fileURLToPath(import.meta.url));
const packageDir = resolve(thisDir, '..');
const repoRoot = resolve(packageDir, '..', '..');
const tgzPath = join(repoRoot, 'resources', 'dsh', 'remote-access.tgz');
const manifestPath = join(repoRoot, 'resources', 'dsh', 'manifest.json');

/** 测试边界观测：记录 webServer.register 调用与 disposer 执行。 */
function makeFakeWebServer() {
  const registeredRoutes: string[] = [];
  return {
    registeredRoutes,
    /** 供 dispose 后断言读取的快照字段（与 brief 断言命名一致）。 */
    snapshot() {
      return { registeredRoutes: [...registeredRoutes] };
    },
    webServer: {
      register(route: WebRoute): () => void {
        registeredRoutes.push(`${route.kind} ${route.path}`);
        return () => {
          const index = registeredRoutes.indexOf(`${route.kind} ${route.path}`);
          if (index >= 0) registeredRoutes.splice(index, 1);
        };
      },
    },
  };
}

/** 等待真实 Cordis 加载收敛；await 完成不代表缺依赖的插件已挂载。 */
async function bootBuiltPlugin({ withProfileContext = true } = {}) {
  const fake = makeFakeWebServer();
  const ctx = new Context();
  // 满足插件 inject 声明：connection 在 T03 不被使用，占位即可。
  ctx.provide('connection', {});
  ctx.provide('webServer', fake.webServer);
  // 宿主路径只指向临时沙箱，绝不借用真实 ~/.dsh。
  const home = withProfileContext ? mkdtempSync(join(tmpdir(), 'tuvish-cordis-host-')) : undefined;
  if (home !== undefined) {
    ctx.provide('profileContext', { home, dir: join(home, 'profiles', 'desktop'), name: 'desktop' });
  }
  const fiber = ctx.plugin(plugin);
  onTestFinished(async () => {
    try {
      await fiber.dispose();
    } finally {
      if (home !== undefined) rmSync(home, { recursive: true, force: true });
    }
  });
  await fiber;
  return { fiber, fake };
}

describe('T03 bundle 包契约', () => {
  it('packed_bundle_boots_and_disposes', async () => {
    // apply 直接调用也应返回 disposer（effect 形态本身可清理）
    const noopServer = makeFakeWebServer();
    const disposer = builtApply({ webServer: noopServer.webServer as never });
    expect(typeof disposer).toBe('function');

    const { fiber, fake } = await bootBuiltPlugin();
    // RED 阶段缺口：骨架不注册任何路由 → 这里失败
    expect(fake.registeredRoutes.length).toBeGreaterThan(0);
    expect(fake.registeredRoutes.join(' ')).toContain('/tuvis-remote/health');

    await fiber.dispose();
    const afterDispose = fake.snapshot();
    expect(afterDispose.registeredRoutes).toEqual([]);
  });

  it('packed_bundle_stays_pending_without_profile_context_and_registers_no_routes', async () => {
    const { fiber, fake } = await bootBuiltPlugin({ withProfileContext: false });
    // Cordis 缺必需服务时 await 仍会完成；不能据此冒充启动成功。
    // FiberState.PENDING = 0；宿主 const enum 不提供运行时导出。
    expect(fiber.state).toBe(0);
    expect(fake.registeredRoutes).toEqual([]);
  });

  it('archive_has_no_lifecycle_script', () => {
    expect(
      existsSync(tgzPath),
      '产物缺失：先运行 pnpm --filter @tuvis/dsh-remote-access build && node scripts/build-dsh-plugin.mjs',
    ).toBe(true);
    expect(existsSync(manifestPath)).toBe(true);

    const archiveEntries = listTarEntries(readFileSync(tgzPath));
    const pkgEntry = archiveEntries.find((entry) => entry.path === 'package/package.json');
    expect(pkgEntry).toBeDefined();
    const packageManifest = JSON.parse(pkgEntry!.data.toString('utf8'));
    expect(packageManifest.name).toBe('@tuvis/dsh-remote-access');

    // brief 最低断言：绝不允许安装期钩子（安装版不需要用户系统构建）
    expect(packageManifest.scripts?.postinstall).toBeUndefined();
    for (const hook of ['preinstall', 'install', 'postinstall', 'prepare'] as const) {
      expect(packageManifest.scripts?.[hook]).toBeUndefined();
    }
    // 更强：打包清单整体不带 scripts（与打包脚本的白名单重建一致）
    expect(packageManifest.scripts).toBeUndefined();
    expect(packageManifest).toEqual(buildPackedPackageManifest());

    // 验收④：dsh.engines 只声明下界，与宿主动态契约准入不冲突；
    // 全清单不得出现精确版本豁免（allow-version / allowVersion）。
    expect(packageManifest.dsh.engines.dsh).toBe('>=0.2.1-rc.3');
    expect(packageManifest.peerDependencies['@deepseek-ai/dsh']).toBe('>=0.2.1-rc.3');
    expect(JSON.stringify(packageManifest)).not.toMatch(/allow[-_]?version/i);
  });

  it('host_react_is_not_bundled', () => {
    const archiveEntries = listTarEntries(readFileSync(tgzPath));
    const paths = archiveEntries.map((entry) => entry.path);
    // brief 最低断言
    expect(paths).not.toContain('node_modules/react/index.js');
    // 更强：任何 node_modules 都不打进包（运行时零依赖，官方 DSH/Cordis 均为 peer）
    expect(paths.some((path) => path.includes('node_modules/'))).toBe(false);
    // 数据面也不得内联 React 产物
    for (const entry of archiveEntries) {
      expect(entry.data.toString('utf8')).not.toMatch(/from\s*["']react["']/);
    }
  });

  it('archive_ships_the_classic_script_client_envelope', () => {
    // 归档里的 lib/client.js 必须是官方信封形态（R03）：宿主下发的是 tgz 里这一份，
    // 只验 lib/ 不验归档会漏掉「打包时拿错文件」这一类漂移。
    const archiveEntries = listTarEntries(readFileSync(tgzPath));
    const clientEntry = archiveEntries.find((entry) => entry.path === 'package/lib/client.js');
    expect(clientEntry, '归档必须含 package/lib/client.js（exports["./client"] 的目标）').toBeDefined();
    const clientSource = clientEntry!.data.toString('utf8');

    expect(clientSource).toContain('__ModuleLoader__');
    expect(clientSource).toContain('id: "@tuvis/dsh-remote-access"');
    expect(clientSource).not.toMatch(/^\s*(?:import|export)\s/m);
    expect(clientSource).toContain('require("react")');

    // 归档不得残留旧 ESM 入口（exports 已指向 client.js）
    expect(archiveEntries.map((entry) => entry.path)).not.toContain('package/lib/local-ui.js');
  });

  it('build_is_byte_reproducible_without_dev_paths', () => {
    const dirA = mkdtempSync(join(tmpdir(), 'tuvish-pack-a-'));
    const dirB = mkdtempSync(join(tmpdir(), 'tuvish-pack-b-'));
    try {
      const first = buildReleaseArtifacts({ outDir: dirA });
      const second = buildReleaseArtifacts({ outDir: dirB });
      // 两次构建逐字节一致
      expect(readFileSync(join(dirA, 'remote-access.tgz')).equals(second.tarball)).toBe(true);
      expect(readFileSync(join(dirB, 'remote-access.tgz')).equals(first.tarball)).toBe(true);
      expect(readFileSync(join(dirA, 'manifest.json')).equals(second.manifest)).toBe(true);

      // 与已提交产物一致（防止归档/清单漂移过期）
      expect(readFileSync(tgzPath).equals(first.tarball)).toBe(true);
      const committedManifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as Record<string, unknown>;
      expect(committedManifest).toEqual({
        pluginVersion: first.version,
        sha256: first.sha256,
        bindingProtocol: BINDING_PROTOCOL,
      });

      // 归档不含绝对开发路径；条目按路径排序（确定性声明）
      const entries = listTarEntries(first.tarball);
      const sortedPaths = entries.map((entry) => entry.path).sort();
      expect(entries.map((entry) => entry.path)).toEqual(sortedPaths);
      for (const entry of entries) {
        expect(entry.data.toString('utf8')).not.toContain(repoRoot);
        expect(entry.data.toString('utf8')).not.toContain('file://');
      }
    } finally {
      rmSync(dirA, { recursive: true, force: true });
      rmSync(dirB, { recursive: true, force: true });
    }
  });
});
