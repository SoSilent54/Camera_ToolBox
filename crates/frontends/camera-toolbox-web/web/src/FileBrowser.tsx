import { useCallback, useEffect, useState } from 'react';
import { listLocalFiles, type DirectoryEntry } from './workflow';

/**
 * 本地文件浏览器：面包屑导航 + 目录列表 + 文件选择。
 * `directory`/`selection` 受控，由父节点读写。
 */
export function FileBrowser({
  root,
  directory,
  selection,
  onDirectory,
  onSelection,
}: {
  root: string;
  directory: string;
  selection: string;
  onDirectory: (path: string) => void;
  onSelection: (path: string) => void;
}) {
  const [entries, setEntries] = useState<DirectoryEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (path: string) => {
    if (!root.trim()) {
      setEntries([]);
      setError('请先设置工作区根目录');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const result = await listLocalFiles(root, path);
      setEntries(result.entries);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setEntries([]);
    } finally {
      setLoading(false);
    }
  }, [root]);

  useEffect(() => {
    void load(directory);
  }, [load, directory]);

  const crumbs = directory.split('/').filter(Boolean);
  const navigateTo = (index: number) => {
    const target = crumbs.slice(0, index + 1).join('/');
    onDirectory(target);
  };

  return (
    <div className="file-browser nodrag">
      <div className="file-browser-crumbs">
        <button type="button" onClick={() => onDirectory('')}>根</button>
        {crumbs.map((crumb, index) => (
          <span key={`${crumb}-${index}`} className="crumb">
            <button type="button" onClick={() => navigateTo(index)}>{crumb}</button>
            <span className="crumb-sep">/</span>
          </span>
        ))}
      </div>
      {loading && <div className="file-browser-hint">加载中…</div>}
      {error && <div className="file-browser-error">{error}</div>}
      {!loading && !error && (
        <ul className="file-browser-list">
          {entries.map((entry) => (
            <li key={entry.path}>
              <button
                type="button"
                className={`file-entry ${entry.isDirectory ? 'is-dir' : ''} ${selection === entry.path ? 'selected' : ''}`}
                onClick={() => (entry.isDirectory ? onDirectory(entry.path) : onSelection(entry.path))}
              >
                <span className="file-entry-icon">{entry.isDirectory ? '▸' : '·'}</span>
                <span className="file-entry-name">{entry.name}</span>
                {!entry.isDirectory && <span className="file-entry-size">{formatSize(entry.size)}</span>}
              </button>
            </li>
          ))}
          {entries.length === 0 && <li className="file-browser-hint">空目录</li>}
        </ul>
      )}
    </div>
  );
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}
