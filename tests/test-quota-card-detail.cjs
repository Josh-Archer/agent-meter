// Numeric quota cards must keep provider.detail (plan name, "N of M used").
const assert = require('node:assert/strict');
const fs = require('node:fs');

const source = fs.readFileSync('gnome-extension/agent-meter@local/extension.js', 'utf8');
const quotaStart = source.indexOf('function quotaWindows(');
const quotaEnd = source.indexOf('function iconFor(', quotaStart);
const colorStart = source.indexOf('const COLOR_STOPS =');
const colorEnd = source.indexOf('function limitText(', colorStart);
const rebuildStart = source.indexOf('    _rebuildDesktop(state) {');
const rebuildEnd = source.indexOf('\n    destroy() {', rebuildStart);
const width = source.match(/const PROGRESS_WIDTH = (\d+);/);
assert.ok(quotaStart >= 0 && quotaEnd > quotaStart, 'quotaWindows must stay extractable');
assert.ok(colorStart >= 0 && colorEnd > colorStart, 'quotaColor must stay extractable');
assert.ok(rebuildStart >= 0 && rebuildEnd > rebuildStart, '_rebuildDesktop must stay extractable');
assert.ok(width, 'PROGRESS_WIDTH must stay extractable');

function actor(type, props) {
    return {
        type,
        props: props ?? {},
        children: [],
        add_child(child) { this.children.push(child); },
        destroy_all_children() { this.children = []; },
    };
}

const St = {
    BoxLayout: function BoxLayout(props) { return actor('box', props); },
    Label: function Label(props) { return actor('label', props); },
    Icon: function Icon(props) { return actor('icon', props); },
    Widget: function Widget(props) { return actor('widget', props); },
};

const Desktop = eval(`
${source.slice(quotaStart, quotaEnd)}
${source.slice(colorStart, colorEnd)}
const PROGRESS_WIDTH = ${width[1]};
function iconFor() { return {}; }
(class {
${source.slice(rebuildStart, rebuildEnd)}
})
`);

function walk(node, visit) {
    visit(node);
    for (const child of node.children ?? [])
        walk(child, visit);
}

function cardsOf(desktop) {
    return desktop.children.filter(child =>
        String(child.props.style_class ?? '').includes('agent-meter-desktop-card'));
}

function labelsOf(node) {
    const texts = [];
    walk(node, item => {
        if (item.type === 'label' && typeof item.props.text === 'string')
            texts.push(item.props.text);
    });
    return texts;
}

function limitTexts(node) {
    const texts = [];
    walk(node, item => {
        if (item.type === 'label' && item.props.style_class === 'agent-meter-limit')
            texts.push(item.props.text);
    });
    return texts;
}

function progressCount(node) {
    let count = 0;
    walk(node, item => {
        if (item.type === 'widget' && item.props.style_class === 'agent-meter-progress')
            count += 1;
    });
    return count;
}

function render(state) {
    const desktop = actor('desktop');
    const view = new Desktop();
    view._desktop = desktop;
    view._extension = {};
    view._finishDrag = () => {};
    view._attachDragHandle = () => {};
    view._syncDesktopVisibility = () => {};
    view._queueClamp = () => {};
    view._rebuildDesktop(state);
    return desktop;
}

const numeric = {
    generated_at: '2026-09-16T20:45:00Z',
    providers: [
        {
            id: 'codex',
            label: 'Codex',
            icon: 'codex',
            status: 'fresh',
            detail: 'Plan: pro',
            windows: [
                {id: 'five_hour', label: '5 hours', remaining_percent: 72, reset_label: 'resets Mon'},
                {id: 'weekly', label: 'Weekly', remaining_percent: 51, reset_label: 'resets Sat'},
            ],
        },
        {
            id: 'copilot',
            label: 'GitHub Copilot',
            icon: 'copilot',
            status: 'fresh',
            detail: 'Premium monthly: 42 of 300 used',
            windows: [
                {id: 'premium_interactions', label: 'Premium monthly', remaining_percent: 86, reset_label: 'Reset unknown'},
            ],
        },
    ],
};

const desktop = render(numeric);
const cards = cardsOf(desktop);
assert.equal(cards.length, 2);

const codexLabels = labelsOf(cards[0]);
assert.ok(codexLabels.includes('Codex'));
assert.ok(codexLabels.includes('5 hours'));
assert.ok(codexLabels.includes('Weekly'));
assert.ok(codexLabels.some(text => text.startsWith('72%')), 'five-hour percent stays on the card');
assert.ok(codexLabels.some(text => text.startsWith('51%')), 'weekly percent stays on the card');
assert.deepEqual(limitTexts(cards[0]), ['Plan: pro']);
assert.equal(progressCount(cards[0]), 2);

const copilotLabels = labelsOf(cards[1]);
assert.ok(copilotLabels.some(text => text.startsWith('86%')));
assert.deepEqual(limitTexts(cards[1]), ['Premium monthly: 42 of 300 used']);
assert.equal(progressCount(cards[1]), 1);

for (const detail of [null, undefined, '']) {
    const blank = render({
        providers: [{
            id: 'codex',
            label: 'Codex',
            status: 'fresh',
            detail,
            windows: [{id: 'weekly', label: 'Weekly', remaining_percent: 40, reset_label: 'resets Sat'}],
        }],
    });
    const card = cardsOf(blank)[0];
    assert.deepEqual(limitTexts(card), [], `empty detail ${String(detail)} must not invent a plan line`);
    assert.equal(progressCount(card), 1);
    assert.ok(!labelsOf(card).includes('Signed in and operational'));
    assert.ok(!labelsOf(card).includes('Active'));
}

const operational = render({
    providers: [{id: 'muse', label: 'Muse', status: 'fresh', windows: []}],
});
const operationalCard = cardsOf(operational)[0];
assert.deepEqual(limitTexts(operationalCard), ['Signed in and operational']);
assert.ok(labelsOf(operationalCard).includes('Active'));
assert.equal(progressCount(operationalCard), 0);

const planOnly = render({
    providers: [{
        id: 'codex',
        label: 'Codex',
        status: 'fresh',
        detail: 'Plan: plus',
        windows: [{id: 'account', label: 'Account', remaining_percent: 100, reset_label: 'Active'}],
    }],
});
const planCard = cardsOf(planOnly)[0];
assert.deepEqual(limitTexts(planCard), ['Plan: plus'], 'non-numeric windows still show the plan once');
assert.equal(progressCount(planCard), 0);

const stale = render({
    providers: [{
        id: 'grok',
        label: 'Grok',
        status: 'stale',
        detail: 'Plan: pro',
        windows: [{id: 'weekly', label: 'Weekly', remaining_percent: 10, reset_label: 'resets Sun'}],
    }],
});
const staleCard = cardsOf(stale)[0];
assert.deepEqual(limitTexts(staleCard), ['Usage unavailable; cached data expired.']);
assert.ok(!labelsOf(staleCard).includes('Plan: pro'));
assert.equal(progressCount(staleCard), 0, 'stale cards keep the unavailable message instead of bars');

const failed = render({
    providers: [{
        id: 'codex',
        label: 'Codex',
        status: 'error',
        detail: 'Codex usage unavailable: timeout',
        windows: [{id: 'weekly', label: 'Weekly', remaining_percent: 10, reset_label: 'resets Sun'}],
    }],
});
const failedCard = cardsOf(failed)[0];
assert.deepEqual(limitTexts(failedCard), ['Codex usage unavailable: timeout']);
assert.equal(progressCount(failedCard), 0);

const waiting = render(null);
assert.deepEqual(labelsOf(waiting), ['⠿  Agent Meter', 'Waiting for local usage data…']);

console.log('Numeric quota cards show plan and N of M used text');
