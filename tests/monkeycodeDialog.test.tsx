import { describe, expect, it } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { I18nProvider } from '../src/i18n';
import { ApiProviderDialog, createProviderDraft, type ProviderCategory } from '../src/pages/ApiAccessPage';

const renderDialog = (category: ProviderCategory) => renderToStaticMarkup(
  <I18nProvider>
    <ApiProviderDialog activeCategory={category} editingRow={null}
      initialDraft={createProviderDraft(category)} busy={false}
      onClose={() => {}} onSave={async () => ({ saved: true })} />
  </I18nProvider>,
);

describe('MonkeyCode advanced settings', () => {
  for (const category of ['openai-compatibility', 'codex-api-key', 'claude-api-key'] as const) {
    it(`offers optional masked signing_secret for ${category}`, () => {
      const html = renderDialog(category);
      const advanced = html.slice(html.indexOf('provider-advanced-settings'));
      expect(advanced).toContain('MonkeyCode 支持');
      expect(advanced).toContain('type="password"');
      expect(advanced).toContain('placeholder="omas_..."');
      expect(advanced).not.toContain('monkeycode.png');
    });
  }
  it('does not offer MonkeyCode signing for Gemini', () => {
    expect(renderDialog('gemini-api-key')).not.toContain('MonkeyCode 支持');
  });
});
