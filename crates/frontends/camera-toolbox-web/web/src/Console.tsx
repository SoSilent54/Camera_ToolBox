import { useState } from 'react';

/** 底部可收起 Console：显示运行时 log（事件），支持折叠与清空。 */
export function Console({ events, onClear }: { events: string[]; onClear: () => void }) {
  const [collapsed, setCollapsed] = useState(false);
  return (
    <footer className={`console ${collapsed ? 'collapsed' : ''}`}>
      <div className="console-header">
        <button type="button" className="console-toggle" onClick={() => setCollapsed((value) => !value)} aria-expanded={!collapsed}>
          <span className="chevron" aria-hidden="true">{collapsed ? '▸' : '▾'}</span>
          Console
        </button>
        <span className="console-count">{events.length} event{events.length === 1 ? '' : 's'}</span>
        <button type="button" className="console-clear" onClick={onClear}>Clear</button>
      </div>
      {!collapsed && (
        <ol className="console-log">
          {events.map((event, index) => <li key={`${event}-${index}`}>{event}</li>)}
        </ol>
      )}
    </footer>
  );
}
