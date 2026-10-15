"""Actual Studio modules and Wasm against deterministic browser HTTP fixtures.

Optional: Python Playwright + Chromium. This harness mounts about:blank, adapts
module URLs/origin, and bundles the exact worker functions into a classic worker.
The actual checked-in Wasm binary executes; this is not a physics mock. It makes
no network requests and does not validate production CSP/module-worker loading,
fgdbd, native authorization, or database execution.
"""
from pathlib import Path
import base64
import os
import re
import unittest
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
TOKEN = 'abcd0123'  # Non-credential fixture.
MAX_VERTEX = str((1 << 128) - 1)


def mount(page):
    html = re.sub(r'<link\b[^>]*>|<script\b[^>]*>.*?</script>', '',
                  (ROOT / 'index.html').read_text(), flags=re.S)
    page.set_content(html)
    page.add_style_tag(content=(ROOT / 'studio.css').read_text())
    sources = {name: (ROOT / name).read_text() for name in
               ['model.mjs', 'layout.mjs', 'demo.mjs', 'graph.mjs', 'experience.mjs', 'app.mjs']}
    sources['wasm'] = 'data:application/wasm;base64,' + base64.b64encode((ROOT / 'wasm/force.wasm').read_bytes()).decode()
    page.evaluate(r'''async sources => {
      window.fixture = {requests: [], pending: [], mode: 'ok', delaySchema: false, physics: [], workerInputs: []};
      const WorkerClass = window.Worker;
      window.Worker = class extends WorkerClass {
        constructor(...args) { super(...args); this.addEventListener('message', ({data}) => fixture.physics.push({engine:data.engine,done:data.done,ticks:data.ticks,error:data.error})); }
        postMessage(data, ...args) { fixture.workerInputs.push(data); super.postMessage(data,...args); }
      };
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
        if (fixture.mode === 'scores') return new Response(JSON.stringify({v:1,seq:'18446744073709551615',columns:['vertex','score'],rows:[[{type:'vertex',value:'340282366920938463463374607431768211455'},{type:'float',value:'0.125'}]]}));
        if (fixture.mode === 'empty') return new Response(JSON.stringify({v:1,columns:['x'],rows:[],seq:13}));
        if (fixture.mode === 'width') return new Response(JSON.stringify({v:1,columns:['x'],rows:[[]],seq:13}));
        if (fixture.mode === 'wide') return new Response(JSON.stringify({v:1,columns:Array.from({length:12},(_,i)=>'c'+i),
          rows:[Array.from({length:12},(_,i)=>({type:'int',value:String(i)})),Array.from({length:12},(_,i)=>({type:'int',value:String(i+1)}))],seq:14}));
        return new Response(JSON.stringify(result()).replace('"seq":12','"seq":18446744073709551615'));
      };
      const blob = text => URL.createObjectURL(new Blob([text], {type:'text/javascript'}));
      const model = blob(sources['model.mjs']);
      const layoutSource = sources['layout.mjs'].replace("new URL('./force.wasm', import.meta.url)", JSON.stringify(sources.wasm));
      const layout = blob(layoutSource.replaceAll("'./model.mjs'",JSON.stringify(model)));
      const demo = blob(sources['demo.mjs']);
      const experience = blob(sources['experience.mjs'].replaceAll("'./model.mjs'",JSON.stringify(model)));
      const classic = blob(sources['model.mjs'].replaceAll('export ', '') + '\n' +
        layoutSource.replace("import { hash, LIMITS } from './model.mjs';", '').replaceAll('export ', ''));
      const graph = blob(sources['graph.mjs']
        .replace("new URL('./layout.mjs',import.meta.url),{type:'module'}", JSON.stringify(classic))
        .replaceAll("'./model.mjs'",JSON.stringify(model)).replaceAll("'./layout.mjs'",JSON.stringify(layout)));
      const app = blob("const location = {origin:'http://127.0.0.1:8080',protocol:'http:'};\n" + sources['app.mjs']
        .replaceAll("'./model.mjs'",JSON.stringify(model)).replaceAll("'./demo.mjs'",JSON.stringify(demo))
        .replaceAll("'./graph.mjs'",JSON.stringify(graph)).replaceAll("'./experience.mjs'",JSON.stringify(experience)));
      await import(app);
    }''', sources)


class ClientState(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.playwright = sync_playwright().start()
        cls.browser = cls.playwright.chromium.launch(executable_path=os.environ.get('CHROMIUM', '/usr/bin/chromium'), headless=True)

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
        self.assertNotIn(TOKEN, self.page.evaluate('JSON.stringify(fixture.workerInputs)'))

    def test_cancelled_response_cannot_replace_a_new_result(self):
        self.connect()
        self.execute('delay')
        self.page.click('#cancel-query')
        self.assertTrue(self.page.evaluate('fixture.requests.at(-1).signal.aborted'))
        self.execute('empty')
        self.assertIn('snapshot 13', self.page.locator('#query-status').inner_text())
        self.release()
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

    def test_actual_wasm_worker_settles_and_controls_reheat_without_requests(self):
        self.page.wait_for_function("fixture.physics.some(p=>p.engine==='wasm'&&p.done)", timeout=30000)
        self.assertEqual(self.page.evaluate('fixture.physics.filter(p=>p.error)'), [])
        self.page.click('#physics-toggle')
        self.page.locator('#physics-charge').fill('125')
        self.page.wait_for_function("fixture.workerInputs.some(p=>p.action==='configure'&&p.settings.charge===125)")
        self.page.click('#layout-stop')
        self.assertIn('paused', self.page.locator('#physics-status').inner_text())
        self.page.click('#layout-reheat')
        self.page.wait_for_function("fixture.physics.at(-1).ticks>0 && !fixture.physics.at(-1).done")
        self.assertEqual(self.page.evaluate('fixture.requests.length'), 0)

    def test_chronicle_comparison_is_result_scoped_and_cleared_on_auth_failure(self):
        self.page.get_by_label('Sample snapshot 101', exact=True).click()
        self.page.click('#pin-baseline')
        self.page.get_by_label('Sample snapshot 104', exact=True).click()
        self.page.click('#compare-toggle')
        text=self.page.locator('#comparison-banner').inner_text()
        self.assertIn('A 101 → B 104',text)
        self.assertIn('Only B: 3 nodes / 6 edges',text)
        self.assertIn('Only A: 0 nodes / 1 edges',text)
        self.assertIn('Changed: 1 nodes',text)
        self.assertIn('absence ≠ deletion',text)
        self.connect()
        self.assertTrue(self.page.locator('#compare-toggle').is_disabled())
        self.page.click('#pin-baseline')
        self.execute('403')
        self.assertIn('No baseline',self.page.locator('#baseline-status').inner_text())
        self.assertTrue(self.page.locator('#comparison-banner').is_hidden())

    def test_temporal_sequence_is_exact_and_loading_never_executes(self):
        self.connect()
        count=self.page.evaluate('fixture.requests.length')
        self.page.fill('#snapshot-input','18446744073709551615')
        self.page.click('#snapshot-load')
        self.assertIn('FOR SYSTEM_TIME AS OF SEQ 18446744073709551615',self.page.input_value('#query-editor'))
        self.assertEqual(self.page.evaluate('fixture.requests.length'),count)
        self.page.fill('#snapshot-input','18446744073709551616')
        self.page.click('#snapshot-load')
        self.assertIn('64-bit',self.page.locator('#toast').inner_text())

    def test_prism_score_overlay_requires_a_baseline_and_keeps_original_rows(self):
        self.connect()
        self.assertTrue(self.page.locator('#analysis-toggle').is_disabled())
        self.page.click('#pin-baseline')
        self.execute('scores')
        self.assertFalse(self.page.locator('#analysis-toggle').is_disabled())
        count=self.page.evaluate('fixture.requests.length')
        self.page.click('#analysis-toggle')
        self.assertIn('PRISM / SCORE',self.page.locator('#comparison-banner').inner_text())
        self.assertEqual(self.page.evaluate('fixture.requests.length'),count)
        self.page.locator('.node-option').click()
        self.assertIn('0.125',self.page.locator('#inspector').inner_text())
        self.page.click('#tab-table')
        self.assertEqual(self.page.locator('#result-table th').count(),2)
        self.page.click('#clear-baseline')
        self.assertTrue(self.page.locator('#analysis-toggle').is_disabled())

    def test_canvas_drag_pins_selected_node_and_keyboard_unpins(self):
        self.page.wait_for_function("fixture.physics.some(p=>p.engine==='wasm'&&p.done)",timeout=30000)
        self.page.locator('.node-option').filter(has_text='GraphRAG').click()
        rect=self.page.locator('#graph').bounding_box()
        x,y=rect['x']+rect['width']/2,rect['y']+rect['height']/2
        self.page.mouse.move(x,y)
        self.page.keyboard.down('Shift')
        self.page.mouse.down()
        self.page.mouse.move(x+70,y+30,steps=8)
        self.page.mouse.up()
        self.page.keyboard.up('Shift')
        self.assertTrue(self.page.evaluate("fixture.workerInputs.filter(p=>p.action==='pin'&&p.node==='100').at(-1).fixed"))
        self.page.locator('.node-option').filter(has_text='GraphRAG').focus()
        self.page.keyboard.press('p')
        self.assertFalse(self.page.evaluate("fixture.workerInputs.filter(p=>p.action==='pin'&&p.node==='100').at(-1).fixed"))

    def test_responsive_layout_and_real_theme_controls(self):
        for width in [390,768,1480]:
            self.page.set_viewport_size({'width':width,'height':1000})
            self.page.wait_for_timeout(60)
            self.assertLessEqual(self.page.evaluate('document.documentElement.scrollWidth'),width)
            rect=self.page.locator('#graph').bounding_box()
            self.assertGreater(rect['width'],250)
            self.assertGreater(rect['height'],200)
        self.page.click('#theme-toggle')
        self.assertEqual(self.page.locator('html').get_attribute('data-theme'),'light')
        self.page.click('#tab-table')
        self.assertGreater(self.page.locator('#result-table tbody tr').count(),0)
        self.page.click('#tab-json')
        self.assertIn('"type"',self.page.locator('#json-result').inner_text())


if __name__ == '__main__':
    unittest.main(verbosity=2)
