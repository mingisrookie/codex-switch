import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export function resolveDocumentationTarget(root, file, target, paths = path) {
  if (!target || /^(?:https?:|mailto:|#)/i.test(target)) return null;
  const decoded = decodeURIComponent(target.split('#', 1)[0]);
  const absolute = paths.resolve(root, paths.dirname(file), decoded);
  const relative = paths.relative(root, absolute);
  if (!relative || paths.isAbsolute(relative) || relative === '..' || relative.startsWith(`..${paths.sep}`)) {
    throw new Error('link escapes repository or points to its root');
  }
  return { absolute, relative: paths.normalize(relative) };
}

export function checkDocumentation(root, files) {
  const tracked = new Set(files.map((value) => path.normalize(value)));
  const markdown = [...tracked].filter((value) => value.toLowerCase().endsWith('.md'));
  const failures = [];
  for (const file of markdown) {
    const text = readFileSync(path.resolve(root, file), 'utf8');
    let fence = null;
    for (const [index, line] of text.split(/\r?\n/).entries()) {
      const marker = /^\s*(`{3,}|~{3,})/.exec(line)?.[1];
      if (marker) {
        if (!fence) fence = marker;
        else if (marker[0] === fence[0] && marker.length >= fence.length) fence = null;
        continue;
      }
      if (fence) continue;
      const targets = [
        ...Array.from(line.matchAll(/\[[^\]]*\]\((<[^>]+>|[^)]+)\)/g), (match) => {
          const value = match[1].trim();
          return value.startsWith('<') ? value.slice(1, value.indexOf('>')) : value.replace(/\s+["'][^"']*["']$/, '');
        }),
        ...Array.from(line.matchAll(/<(?:img|a)\b[^>]*\b(?:src|href)=["']([^"']+)["'][^>]*>/gi), (match) => match[1]),
      ];
      for (const target of targets) {
        try {
          const resolved = resolveDocumentationTarget(root, file, target);
          if (!resolved) continue;
          if (!existsSync(resolved.absolute)) throw new Error('missing link target');
          if (!tracked.has(resolved.relative)) throw new Error('link target is not tracked');
        } catch (error) {
          failures.push(`${file}:${index + 1}: ${error.message}: ${target}`);
        }
      }
    }
  }
  return { markdown: markdown.length, failures };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const root = process.cwd();
  const files = execFileSync('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard'], { cwd: root })
    .toString('utf8').split('\0').filter(Boolean);
  const result = checkDocumentation(root, files);
  if (result.failures.length) {
    console.error(result.failures.join('\n'));
    process.exitCode = 1;
  } else console.log(`DOC_LINKS_OK markdown=${result.markdown}`);
}
