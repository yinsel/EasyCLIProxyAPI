import { afterEach, describe, expect, it, spyOn } from 'bun:test';
import { deleteProviderRecord, type ProviderRecordIdentity } from '../src/pages/ApiAccessPage';
import { managementApi } from '../src/services/managementApi';

const target: ProviderRecordIdentity = {
  section: 'codex-api-key',
  index: 0,
  name: 'Codex API',
  apiKey: 'shared-key',
  baseUrl: 'https://upstream.example/v1',
};

const spies: ReturnType<typeof spyOn>[] = [];
afterEach(() => {
  spies.splice(0).forEach((spy) => spy.mockRestore());
});

// The desktop bridge decodes GET URLs, but the core matches DELETE queries
// against its stored URL and returns success even when no credential matches.
function mockCore(section: string, initial: Record<string, unknown>[]) {
  let records = structuredClone(initial);
  const get = spyOn(managementApi, 'get').mockImplementation(async (path) => {
    expect(path).toBe(`/${section}`);
    return {
      [section]: records.map(({ upstream, ...record }) => (
        upstream ? { ...record, 'base-url': upstream } : record
      )),
    } as never;
  });
  const remove = spyOn(managementApi, 'delete').mockImplementation(async (path, options) => {
    expect(path).toBe(`/${section}`);
    const query = options?.query ?? {};
    if (query['api-key']) {
      records = records.filter((record) => (
        record['api-key'] !== query['api-key'] || record['base-url'] !== query['base-url']
      ));
    } else if (query.name) {
      records = records.filter((record) => record.name !== query.name);
    } else if (query.index !== undefined) {
      records.splice(Number(query.index), 1);
    } else {
      throw new Error('missing deletion identity');
    }
    return { status: 'ok' } as never;
  });
  spies.push(get, remove);
  return { get, remove, records: () => records };
}

describe('provider deletion', () => {
  it('deletes only the selected shared-credential configuration after a reorder and returns remaining remark records', async () => {
    const first = { 'api-key': target.apiKey, 'base-url': target.baseUrl, priority: 10, 'auth-index': 'first' };
    const second = { ...first, priority: 1, 'auth-index': 'second' };
    const core = mockCore(target.section, [second, first]);

    const remaining = await deleteProviderRecord({ ...target, index: 1, record: second });

    expect(core.remove).toHaveBeenCalledWith('/codex-api-key', { query: { index: 0 } });
    expect(core.records()).toEqual([first]);
    expect(remaining).toEqual([{ 'api-key': target.apiKey, 'base-url': target.baseUrl, priority: 10 }]);
  });

  it('refuses a changed configuration with the same credentials', async () => {
    const record = { 'api-key': target.apiKey, 'base-url': target.baseUrl, priority: 10 };
    const core = mockCore(target.section, [{ ...record, priority: 1 }]);
    await expect(deleteProviderRecord({ ...target, record })).rejects.toThrow();
    expect(core.remove).not.toHaveBeenCalled();
  });

  it('refuses indistinguishable full-record matches', async () => {
    const record = { 'api-key': target.apiKey, 'base-url': target.baseUrl, priority: 10 };
    const core = mockCore(target.section, [record, record]);
    await expect(deleteProviderRecord({ ...target, record })).rejects.toThrow();
    expect(core.remove).not.toHaveBeenCalled();
  });

  it('removes the last MonkeyCode Codex entry despite its stored bridge URL', async () => {
    const core = mockCore(target.section, [{
      'api-key': target.apiKey,
      'base-url': 'http://127.0.0.1:54321/monkeycode/signed/v1',
      upstream: target.baseUrl,
      signing_secret: 'test-signing-secret',
    }]);

    await deleteProviderRecord(target);

    expect(core.records()).toEqual([]);
    expect(core.remove).toHaveBeenCalledWith('/codex-api-key', { query: { index: 0 } });
  });

  it('resolves the current full-list index and preserves DeepSeek and same-key endpoints', async () => {
    const deepseek = { name: 'DeepSeek', 'api-key': 'deepseek-key', 'base-url': 'https://api.deepseek.com' };
    const other = { 'api-key': target.apiKey, 'base-url': 'https://other.example/v1' };
    const core = mockCore(target.section, [deepseek, other, {
      'api-key': target.apiKey,
      'base-url': target.baseUrl,
    }]);

    await deleteProviderRecord(target);

    expect(core.records()).toEqual([deepseek, other]);
    expect(core.remove).toHaveBeenCalledWith('/codex-api-key', { query: { index: 2 } });
  });

  it('refuses a stale row even if another endpoint uses its key and old index', async () => {
    const other = { 'api-key': target.apiKey, 'base-url': 'https://other.example/v1' };
    const core = mockCore(target.section, [other]);

    await expect(deleteProviderRecord(target)).rejects.toThrow();

    expect(core.remove).not.toHaveBeenCalled();
    expect(core.records()).toEqual([other]);
  });

  it('refuses ambiguous matching records', async () => {
    const record = { 'api-key': target.apiKey, 'base-url': target.baseUrl };
    const core = mockCore(target.section, [record, record]);

    await expect(deleteProviderRecord(target)).rejects.toThrow();

    expect(core.remove).not.toHaveBeenCalled();
    expect(core.records()).toHaveLength(2);
  });

  it('does not delete when refreshing the provider list fails', async () => {
    const core = mockCore(target.section, []);
    core.get.mockRejectedValue(new Error('core unavailable'));

    await expect(deleteProviderRecord(target)).rejects.toThrow('core unavailable');
    expect(core.remove).not.toHaveBeenCalled();
  });

  it('propagates deletion failures instead of reporting success', async () => {
    const core = mockCore(target.section, [{ 'api-key': target.apiKey, 'base-url': target.baseUrl }]);
    core.remove.mockRejectedValue(new Error('write failed'));

    await expect(deleteProviderRecord(target)).rejects.toThrow('write failed');
    expect(core.records()).toHaveLength(1);
  });

  it('handles an already removed last record returned as null', async () => {
    const core = mockCore(target.section, []);
    core.get.mockResolvedValue({ [target.section]: null } as never);

    await expect(deleteProviderRecord(target)).rejects.toThrow();
    expect(core.remove).not.toHaveBeenCalled();
  });

  for (const section of ['claude-api-key', 'gemini-api-key', 'openai-compatibility'] as const) {
    it(`retains deletion support for ${section}`, async () => {
      const core = mockCore(section, [{
        name: target.name,
        'api-key': target.apiKey,
        'base-url': target.baseUrl,
      }]);

      await deleteProviderRecord({ ...target, section });

      expect(core.records()).toEqual([]);
    });
  }
});
