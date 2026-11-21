async page => {
  const base = new URL(page.url()).origin;
  const project = `keys${Date.now().toString(36)}`;
  const names = ['plain', 'keeps-j', 'top'].map(n => `${project}:${n}`);
  const [plain, keepsJ, top] = names;
  const results = [];
  const assert = (ok, message) => { if (!ok) throw Error(message); };
  const post = async (path, data) => {
    const response = await page.request.post(base + path, {data:{by:'agent', ...data}});
    assert(response.ok(), `${path}: ${await response.text()}`);
    return response.json();
  };
  const entry = name => page.locator(`.entry[data-name="${name}"]`);
  const inside = name => entry(name).frameLocator('iframe');
  const state = () => page.evaluate(() => ({
    sel: document.querySelector('.entry.sel')?.dataset.name ?? null,
    dock: !document.getElementById('dock-panel').hidden,
    nopinned: document.getElementById('app').classList.contains('nopinned'),
    commenting: document.querySelector('.entry.sel [data-act="comment"]')?.classList.contains('on') ?? false,
    // The page's activeElement can lag behind a click from one frame into another; the frames know.
    active: [...document.querySelectorAll('iframe')].filter(f => f.contentWindow?.document.hasFocus()).map(f => `IFRAME[${f.title}]`)[0] ?? document.activeElement.tagName,
  }));
  const until = async (label, want) => {
    const deadline = Date.now() + 3000;
    let s;
    do {
      s = await state();
      if (Object.entries(want).every(([k, v]) => s[k] === v)) return results.push(`${label}: ${JSON.stringify(s)}`);
      await page.waitForTimeout(50);
    } while (Date.now() < deadline);
    throw Error(`${label}: expected ${JSON.stringify(want)}, got ${JSON.stringify(s)}`);
  };
  const unchanged = async (label, before) => {
    await page.waitForTimeout(400);
    const after = await state();
    for (const k of ['sel', 'dock', 'nopinned', 'commenting']) assert(after[k] === before[k], `${label}: ${k} changed from ${before[k]} to ${after[k]}`);
    results.push(`${label}: unchanged`);
  };
  const fcomment = on => page.waitForFunction(on => document.querySelector('#fcomment').classList.contains('on') === on, on);
  // Selecting and focusing scroll the stream smoothly; a click placed while it still moves lands on the wrong jelly.
  const scrollSettled = async () => {
    for (let last = null;;) {
      const now = await page.evaluate(() => [...document.querySelectorAll('.entry')].map(e => Math.round(e.getBoundingClientRect().top)).join());
      if (now === last) return;
      last = now;
      await page.waitForTimeout(100);
    }
  };
  const focusPage = async () => { await scrollSettled(); await entry(top).locator('h2').click(); assert((await state()).active === 'BODY', 'page has focus'); };
  const focusInside = async name => {
    await scrollSettled();
    await inside(name).locator('#text').click();
    const s = await state();
    assert(s.active === `IFRAME[${name}]`, `focus inside ${name}, got ${s.active}`);
    await until(`a click into ${name.split(':')[1]} selects it`, {sel: name});
  };
  try {
    const view = '<!doctype html><p id="text" style="height:120px">Some words to click</p><input id="field">'
      + '<button id="btn" onclick="this.dataset.clicks = (+this.dataset.clicks || 0) + 1">Act</button> <a id="link" href="#x">Link</a>'
      + '<details><summary id="sum">More</summary>Hidden</details>';
    for (const name of names) {
      const keeps = name === keepsJ ? '<script>addEventListener("keydown", e => { if (e.key === "j") e.preventDefault(); });</script>' : '';
      await post('/api/jellies', {name, about:name, view: view + keeps, verbs:'const verbs = {};', state:{}});
    }
    await page.setViewportSize({width:1280, height:900});
    await page.goto(`${base}/p/${project}`);
    await entry(plain).waitFor();
    await page.waitForFunction(() => [...document.querySelectorAll('.entry iframe')].every(f => f.contentWindow.jelly));

    // Shortcuts while a jelly has focus: selection, dock and pinned column.
    await focusPage();
    await until('select newest', {sel: top});
    await focusInside(top);
    await page.keyboard.press('j'); await until('j inside a jelly moves down', {sel: keepsJ});
    assert((await state()).active === `IFRAME[${top}]`, 'forwarded j leaves focus in the jelly');
    await page.keyboard.press('k'); await until('k inside a jelly moves up', {sel: top});
    await page.keyboard.press('j'); await until('j moves the ring away from the focused jelly', {sel: keepsJ});
    await focusInside(top);
    await page.keyboard.press('a'); await until('a inside a jelly opens the dock', {dock: true});
    await page.keyboard.press('a'); await until('a again closes it', {dock: false});
    await page.keyboard.press('p'); await until('p inside a jelly hides pinned', {nopinned: true});
    await page.keyboard.press('p'); await until('p again shows pinned', {nopinned: false});

    // Arrows and Enter inside a jelly; Enter opens the jelly that has focus.
    await page.keyboard.press('ArrowDown'); await until('ArrowDown inside a jelly moves down', {sel: keepsJ});
    await page.keyboard.press('ArrowUp'); await until('ArrowUp inside a jelly moves up', {sel: top});
    await focusInside(keepsJ);
    await page.keyboard.press('Enter');
    await page.locator('#full').waitFor({state:'visible'});
    assert(await page.evaluate(() => document.querySelector('#full iframe').title) === keepsJ, 'Enter opens the jelly that has focus');
    results.push('Enter inside a jelly opens that jelly');
    // Focus stays in the stream frame; its keys must still reach the page while the full view is open.
    assert((await state()).active === `IFRAME[${keepsJ}]`, 'focus stays in the stream frame after Enter');
    await page.keyboard.press('a'); await until('a after a forwarded Enter opens the dock', {dock: true});
    await page.keyboard.press('a'); await until('a after a forwarded Enter closes it', {dock: false});
    await page.keyboard.press('c'); await fcomment(true);
    await page.keyboard.press('c'); await fcomment(false);
    results.push('c after a forwarded Enter toggles comment mode');
    await page.keyboard.press('Escape');
    await page.locator('#full').waitFor({state:'hidden'});
    await until('closing the full view selects it', {sel: keepsJ});

    // Comment mode begun in the stream survives a forwarded Enter; Escape ends it on the stream button, and c works again.
    await focusInside(keepsJ);
    await page.keyboard.press('c'); await until('c inside the stream turns comment mode on', {commenting: true, sel: keepsJ});
    await page.keyboard.press('Enter');
    await page.locator('#full').waitFor({state:'visible'});
    await fcomment(false);
    await page.keyboard.press('Escape'); await until('Escape ends stream comment mode behind the full view', {commenting: false});
    assert(await page.locator('#full').isVisible(), 'the full view stays open after Escape ends comment mode');
    await page.keyboard.press('Escape');
    await page.locator('#full').waitFor({state:'hidden'});
    await page.keyboard.press('c'); await until('c after that sequence turns comment mode on again', {commenting: true});
    await page.keyboard.press('c'); await until('c turns it off again', {commenting: false});

    // Repeated j inside one jelly walks the stream from the selection, and Enter opens the jelly the ring marks.
    await focusPage();
    await until('clicking the top entry selects it', {sel: top});
    await focusInside(top);
    await page.keyboard.press('j'); await until('first j inside a jelly moves down', {sel: keepsJ});
    await page.keyboard.press('j'); await until('second j inside the same jelly moves on', {sel: plain});
    await page.keyboard.press('Enter');
    await page.locator('#full').waitFor({state:'visible'});
    assert(await page.evaluate(() => document.querySelector('#full iframe').title) === plain, 'Enter after j opens the jelly the ring marks');
    results.push('Enter after j opens the jelly the ring marks');
    await page.keyboard.press('Escape');
    await page.locator('#full').waitFor({state:'hidden'});
    await until('closing the full view selects the opened jelly', {sel: plain});
    await focusInside(keepsJ);
    await page.keyboard.press('c'); await until('c inside an unselected jelly comments on that jelly', {sel: keepsJ, commenting: true});
    await page.keyboard.press('c'); await until('c again turns it off', {commenting: false});

    // Escape inside a jelly closes the dock, and only the dock.
    await focusInside(top);
    await page.keyboard.press('a'); await until('a inside a jelly opens the dock again', {dock: true, sel: top});
    await page.keyboard.press('Escape'); await until('Escape inside a jelly closes the dock', {dock: false});

    // Enter on a control inside a jelly belongs to the jelly.
    for (const id of ['btn', 'link', 'sum']) {
      await inside(top).locator('#' + id).focus();
      await page.keyboard.press('Enter');
      await page.waitForTimeout(400);
      assert(await page.locator('#full').isHidden(), `Enter on #${id} does not open the full view`);
    }
    assert(await inside(top).locator('#btn').getAttribute('data-clicks') === '1', 'the button was activated');
    assert(await inside(top).locator('details').evaluate(d => d.open), 'the summary was toggled');
    results.push('Enter on a button, link and summary stays in the jelly');

    // Caps Lock reports an upper-case key, on the page and inside a jelly.
    await focusPage();
    await page.keyboard.press('J'); await until('J with page focus moves down', {sel: keepsJ});
    await page.keyboard.press('K'); await until('K with page focus moves up', {sel: top});
    await focusInside(top);
    await page.keyboard.press('J'); await until('J inside a jelly moves down', {sel: keepsJ});
    await page.keyboard.press('K'); await until('K inside a jelly moves up', {sel: top});

    // A modifier makes it the browser's or the jelly's key, not a shortcut.
    const modified = await state();
    for (const combo of ['Control+j', 'Alt+j', 'Meta+j', 'Control+a', 'Alt+a', 'Meta+a']) {
      await page.keyboard.press(combo);
      await unchanged(`${combo} inside a jelly`, modified);
    }
    await focusPage();

    // c moves focus into the jelly; a second c must still turn comment mode off.
    await page.keyboard.press('c'); await until('c turns comment mode on', {commenting: true});
    assert((await state()).active === `IFRAME[${top}]`, 'comment mode focuses the jelly');
    await page.keyboard.press('c'); await until('c inside the jelly turns it off', {commenting: false});
    await page.keyboard.press('j'); await until('j still works afterwards', {sel: keepsJ});
    await page.keyboard.press('k'); await until('k back to the top', {sel: top});

    // Typing into a jelly's own input is not a shortcut.
    await inside(top).locator('#field').click();
    const typed = await state();
    await page.keyboard.type('jkacp');
    await unchanged('letters typed into an input', typed);
    assert(await inside(top).locator('#field').inputValue() === 'jkacp', 'input received the letters');

    // A jelly that handles a key itself keeps it.
    await focusPage();
    await page.keyboard.press('j'); await until('select the jelly that keeps j', {sel: keepsJ});
    await focusInside(keepsJ);
    const kept = await state();
    await page.keyboard.press('j');
    await unchanged('j prevented by the jelly', kept);
    await page.keyboard.press('k'); await until('k is still forwarded', {sel: top});

    // Escape typed into a jelly's input, or prevented by the jelly, stays there like any other key.
    await page.keyboard.press('a'); await until('a opens the dock before the Escape checks', {dock: true});
    await inside(top).locator('#field').click();
    const dockOpen = await state();
    await page.keyboard.press('Escape'); await unchanged('Escape typed into an input', dockOpen);
    await inside(keepsJ).locator('body').evaluate(() => addEventListener('keydown', e => { if (e.key === 'Escape') e.preventDefault(); }));
    await focusInside(keepsJ);
    const keepsEscape = await state();
    await page.keyboard.press('Escape'); await unchanged('Escape prevented by the jelly', keepsEscape);
    await focusInside(plain);
    await page.keyboard.press('Escape'); await until('Escape inside another jelly closes the dock', {dock: false});

    // Escape while a comment pop is open closes the pop and goes no further, wherever focus is in the view.
    await page.keyboard.press('a'); await until('a opens the dock before the pop check', {dock: true});
    await page.keyboard.press('c'); await until('c inside the jelly turns comment mode on for the pop', {commenting: true, sel: plain});
    await inside(plain).locator('#text').click({position:{x:10, y:10}});
    await inside(plain).locator('.jf-pop textarea').waitFor();
    await until('a click in comment mode opens a pop', {commenting: false});
    await inside(plain).locator('.jf-pop textarea').evaluate(t => t.blur());
    assert((await state()).active === `IFRAME[${plain}]`, 'focus is in the view but outside the pop');
    await page.keyboard.press('Escape');
    await inside(plain).locator('.jf-pop').waitFor({state:'detached'});
    await until('Escape outside the pop closes the pop and leaves the dock', {dock: true});
    await page.keyboard.press('Escape'); await until('the next Escape closes the dock', {dock: false});

    // Full view: shortcuts after a click into the frame, and Escape ordering.
    await focusPage();
    await page.keyboard.press('Enter');
    await page.locator('#full').waitFor({state:'visible'});
    await page.waitForFunction(name => document.querySelector('#full iframe')?.contentWindow.jelly?.name === name, top);
    await page.frameLocator('#full iframe').locator('#text').click();
    assert((await state()).active === `IFRAME[${top}]`, 'focus inside the full view');
    await page.keyboard.press('a'); await until('a in the full view opens the dock', {dock: true});
    await page.keyboard.press('a'); await until('a in the full view closes it', {dock: false});
    await page.keyboard.press('a'); await until('a in the full view opens the dock again', {dock: true});
    await page.keyboard.press('Escape'); await until('Escape in the full view closes the dock first', {dock: false});
    assert(await page.locator('#full').isVisible(), 'the full view stays open');
    await page.keyboard.press('c'); await fcomment(true);
    await page.keyboard.press('c'); await fcomment(false);
    results.push('c inside the full view toggles comment mode');
    await page.keyboard.press('c'); await fcomment(true);
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('#fcomment').classList.contains('on') && !document.querySelector('#full').hidden);
    results.push('Escape in comment mode only ends comment mode');
    await page.keyboard.press('Escape');
    await page.locator('#full').waitFor({state:'hidden'});
    results.push('second Escape closes the full view');
    return results;
  } catch (e) {
    e.message += `
after ${results.length} steps; last: ${results.at(-1)}`;
    throw e;
  } finally {
    await page.goto('about:blank');
    for (const name of names) await page.request.delete(`${base}/api/jellies/${name}?by=agent`);
  }
}
