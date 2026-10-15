"""Client-state regressions against deterministic HTTP-response fixtures.

Optional developer test: Python Playwright + an installed Chromium browser.
This loads the real embedded HTML/CSS/modules into about:blank, adapts module
URLs to blobs, bundles the production layout functions into a classic worker,
and supplies a lexical test origin plus a fixture fetch. It makes NO network
requests. It tests UI/client ownership, NOT fgdbd, Warden, CSP, native HTTP,
production module-worker loading, or the database query language.

Run: CHROMIUM=/usr/bin/chromium python path/to/tests/browser_client.py
"""
from pathlib import Path
import os
import re
import unittest
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
TOKEN = 'abcd0123'  # Deliberately non-credential fixture.
MAX_VERTEX = str((1 << 128) - 1)


def mount(page):
    html = re.sub(r'<link\b[^>]*>|<script\b[^>]*>.*?</script>', '',
                  (ROOT / 'index.html').read_text(), flags=re.S)
    page.set_content(html)
    page.add_style_tag(content=(ROOT / 'studio.css').read_text())
    sources = {name: (ROOT / name).read_text() for name in
               ['model.mjs', 'layout.mjs', 'demo.mjs', 'graph.mjs', 'app.mjs']}
    page.evaluate(r'''async sources => {
      window.fixture = {requests: [], pending: [], mode: 'ok', delaySchema: false};
      const result = () => ({v:1,columns:['node','node_labels','node_properties'],rows:[[
        {type:'vertex',value:'340282366920938463463374607431768211455'},
        {type:'list',value:[{type:'text',value:'Person'}]},
        {type:'map',value:{name:{type:'text',value:'<img src=x onerror="window.injected=true">'},
                         balance:{type:'int',value:'9007199254740993'}}}]],seq:12});
      window.fetch = async (url, options) => {
        fixture.requests.push({url, ...options});
        if (url.endsWith('/schema')) {
          const response = () => new Response(JSON.stringify({v:1,labels:['Person'],relations:[],properties:['name','balance']}));
          if (fixture.delaySchema) return new Promise(resolve => fixture.pending.push(() => resolve(response())));
          return response();
        }
        if (fixture.mode === 'delay') return new Promise(resolve => fixture.pending.push(() => resolve(new Response(JSON.stringify(result())))));
        if (fixture.mode === '403') return new Response('<html>Forbidden</html>', {status:403});
        if (fixture.mode === '401-oversize') return new Response('denied', {status:401,headers:{'content-length':'9000000'}});
        if (fixture.mode === 'network') throw new TypeError('Fixture connection lost');
        if (fixture.mode === 'empty') return new Response(JSON.stringify({v:1,columns:['x'],rows:[],seq:13}));
        if (fixture.mode === 'width') return new Response(JSON.stringify({v:1,columns:['x'],rows:[[]],seq:13}));
        if (fixture.mode === 'wide') return new Response(JSON.stringify({v:1,columns:Array.from({length:12},(_,i)=>'c'+i),
          rows:[Array.from({length:12},(_,i)=>({type:'int',value:String(i)})),Array.from({length:12},(_,i)=>({type:'int',value:String(i+1)}))],seq:14}));
        return new Response(JSON.stringify(result()).replace('"seq":12','"seq":18446744073709551615'));
      };
      const blob = text => URL.createObjectURL(new Blob([text], {type:'text/javascript'}));
      const model = blob(sources['model.mjs']);
      const layout = blob(sources['layout.mjs'].replaceAll("'./model.mjs'",JSON.stringify(model)));
      const demo = blob(sources['demo.mjs']);
      const classic = blob(sources['model.mjs'].replaceAll('export ', '') + '\n' +
        sources['layout.mjs'].replace("import { hash, LIMITS } from './model.mjs';", '').replaceAll('export ', ''));
      const graph = blob(sources['graph.mjs']
        .replace("new URL('./layout.mjs', import.meta.url), { type: 'module' }", `new URL('${classic}'), { type: 'classic' }`)
        .replaceAll("'./model.mjs'",JSON.stringify(model)).replaceAll("'./layout.mjs'",JSON.stringify(layout)));
      const app = blob("const location = {origin:'http://127.0.0.1:8080',protocol:'http:'};\n" + sources['app.mjs']
        .replaceAll("'./model.mjs'",JSON.stringify(model)).replaceAll("'./demo.mjs'",JSON.stringify(demo)).replaceAll("'./graph.mjs'",JSON.stringify(graph)));
      await import(app);
    }''', sources)


class ClientState(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.playwright = sync_playwright().start()
        executable = os.environ.get('CHROMIUM')
        cls.browser = cls.playwright.chromium.launch(
            **({'executable_path': executable} if executable else {}), headless=True)

    @classmethod
    def tearDownClass(cls):
        cls.browser.close()
        cls.playwright.stop()

    def setUp(self):
        self.page = self.browser.new_page(viewport={'width': 1480, 'height': 1000})
        self.page.set_default_timeout(5000)
        self.errors = []
        self.page.on('pageerror', lambda error: self.errors.append(str(error)))
        mount(self.page)

    def tearDown(self):
        try:
            self.assertEqual(self.errors, [], 'Client must not throw uncaught errors')
        finally:
            self.page.close()

    def connect(self, name='graph'):
        self.page.click('#connect-button')
        self.page.fill('#database-name', name)
        self.page.fill('#capability-token', TOKEN)
        self.page.click('#connect-submit')
        self.idle()

    def idle(self):
        self.page.wait_for_function("!document.getElementById('connect-dialog').open && document.getElementById('run-label').textContent === 'Run query'")

    def execute(self, mode='ok'):
        self.page.evaluate('(mode) => fixture.mode = mode', mode)
        self.page.click('#run-query')
        if mode != 'delay':
            self.page.wait_for_function("document.getElementById('cancel-query').hidden")

    def release(self):
        self.page.evaluate('fixture.pending.shift()()')
        self.page.wait_for_timeout(80)

    def test_exact_parameters_identity_snapshot_and_text_rendering(self):
        self.page.fill('#query-editor', 'RETURN $large')
        self.page.click('#parameters-toggle')
        self.page.fill('#query-parameters', '{"large":9007199254740993}')
        self.connect()
        requests = self.page.evaluate('fixture.requests.map(({signal,...request}) => request)')
        self.assertEqual([r['url'] for r in requests], ['/v1/databases/graph/schema', '/v1/databases/graph/query'])
        self.assertEqual(requests[1]['body'], '{"statement":"RETURN $large","parameters":{"large":9007199254740993}}')
        for request in requests:
            self.assertEqual(request['headers']['Authorization'], 'Bearer ' + TOKEN)
            self.assertEqual(request['credentials'], 'omit')
            self.assertEqual(request['redirect'], 'error')
            self.assertEqual(request['cache'], 'no-store')
        self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '1')
        self.assertIn('18446744073709551615', self.page.locator('#snapshot-status').inner_text())
        self.page.locator('.node-option').click()
        self.assertEqual(self.page.locator('.identity code').inner_text(), MAX_VERTEX)
        self.assertIn('<img src=x', self.page.locator('.element-title').inner_text())
        self.assertEqual(self.page.locator('#inspector img').count(), 0)
        self.assertFalse(self.page.evaluate('Boolean(window.injected)'))
        self.assertEqual(self.page.input_value('#capability-token'), '')

    def test_cancelled_response_cannot_replace_a_new_result(self):
        self.connect()
        self.execute('delay')
        self.page.click('#cancel-query')
        self.assertTrue(self.page.evaluate('fixture.requests.at(-1).signal.aborted'))
        self.execute('empty')
        self.assertIn('snapshot 13', self.page.locator('#query-status').inner_text())
        self.release()  # Fixture deliberately ignores abort to test the generation fence.
        self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '0')
        self.assertIn('snapshot 13', self.page.locator('#query-status').inner_text())
        self.assertEqual(self.page.locator('#history-count').inner_text(), '2')

    def test_opening_sample_discards_late_live_output_and_credentials(self):
        self.connect()
        self.execute('delay')
        self.page.click('#load-sample')
        self.release()
        self.assertEqual(self.page.locator('#dataset-badge').inner_text(), 'SAMPLE DATA')
        self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '32')
        self.assertEqual(self.page.locator('#history-count').inner_text(), '0')
        self.assertEqual(self.page.locator('#connection-label').inner_text(), 'Connect database')

    def test_result_schema_change_resets_local_table_sort(self):
        self.connect()
        self.execute('wide')
        self.page.click('#tab-table')
        self.page.locator('#result-table th button').nth(11).click()
        self.page.click('#load-sample')
        self.page.click('#tab-table')
        self.assertEqual(self.page.locator('#table-count').inner_text(), '59 rows')
        self.assertEqual(self.page.locator('#result-table tbody tr').count(), 50)

    def test_non_json_permission_denial_clears_session(self):
        self.connect()
        self.execute('403')
        self.assertEqual(self.page.locator('#connection-label').inner_text(), 'Connect database')
        self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '—')
        self.assertEqual(self.page.locator('#history-count').inner_text(), '0')
        self.page.click('#nav-schema')
        self.assertNotIn('Person', self.page.locator('#schema-content').inner_text())
        self.page.click('#nav-explore')
        before = self.page.evaluate('fixture.requests.length')
        self.page.click('#run-query')
        self.assertTrue(self.page.locator('#connect-dialog').is_visible())
        self.assertEqual(self.page.evaluate('fixture.requests.length'), before)

    def test_oversized_unauthorized_response_still_clears_session(self):
        self.connect()
        self.execute('401-oversize')
        self.assertEqual(self.page.locator('#connection-label').inner_text(), 'Connect database')
        self.assertIn('8 MiB', self.page.locator('#query-status').inner_text())

    def test_network_and_malformed_result_fail_without_old_data(self):
        self.connect()
        for mode in ['network', 'width']:
            self.execute(mode)
            self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '—')
            self.assertEqual(self.page.locator('#connection-label').inner_text(), 'graph')
            self.assertTrue(self.page.locator('#export-button').is_disabled())
            self.assertIn('error', self.page.locator('#query-status').get_attribute('class'))
        self.execute('ok')
        self.assertEqual(self.page.locator('#metric-nodes').inner_text(), '1')

    def test_closing_pending_connect_prevents_late_schema_from_selecting_database(self):
        self.page.evaluate('fixture.delaySchema = true')
        self.page.click('#connect-button')
        self.page.fill('#capability-token', TOKEN)
        self.page.click('#connect-submit')
        self.page.wait_for_function('fixture.pending.length === 1')
        self.page.keyboard.press('Escape')
        self.page.wait_for_function("!document.getElementById('connect-dialog').open")
        self.assertEqual(self.page.input_value('#capability-token'), '')
        self.page.evaluate('fixture.delaySchema = false')
        self.connect('new-graph')
        self.release()
        self.assertEqual(self.page.locator('#connection-label').inner_text(), 'new-graph')
        self.assertEqual(self.page.evaluate("fixture.requests.filter(r => r.url.endsWith('/query')).length"), 1)

    def test_history_load_does_not_execute_and_pagehide_clears_sensitive_state(self):
        self.connect()
        before = self.page.evaluate('fixture.requests.length')
        self.page.click('#nav-history')
        self.page.locator('.history-entry').click()
        self.assertEqual(self.page.evaluate('fixture.requests.length'), before)
        self.page.evaluate("window.dispatchEvent(new PageTransitionEvent('pagehide'))")
        self.assertEqual(self.page.locator('#connection-label').inner_text(), 'Connect database')
        self.assertEqual(self.page.input_value('#query-editor'), '')
        self.assertEqual(self.page.input_value('#query-parameters'), '{}')
        self.assertEqual(self.page.locator('#history-count').inner_text(), '0')
        self.assertTrue(self.page.locator('#export-button').is_disabled())

    def test_no_capability_or_invalid_parameters_issue_no_http_request(self):
        self.assertEqual(self.page.evaluate('fixture.requests.length'), 0)
        self.page.click('#run-query')
        self.page.fill('#capability-token', 'not-a-hex-token')
        self.page.click('#connect-submit')
        self.assertEqual(self.page.evaluate('fixture.requests.length'), 0)
        self.page.keyboard.press('Escape')
        self.connect()
        before = self.page.evaluate('fixture.requests.length')
        self.page.click('#parameters-toggle')
        self.page.fill('#query-parameters', '[1,2,3]')
        self.page.click('#run-query')
        self.assertEqual(self.page.evaluate('fixture.requests.length'), before)
        self.assertIn('JSON object', self.page.locator('#query-status').inner_text())


if __name__ == '__main__':
    unittest.main(verbosity=2)
