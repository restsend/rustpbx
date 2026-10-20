import { copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';

const vendor = new URL('../static/vendor/', import.meta.url);
const chart = new URL('node_modules/chart.js/', import.meta.url);
await mkdir(vendor, { recursive: true });
const source = await readFile(new URL('dist/chart.umd.min.js', chart), 'utf8');
// The source map is a development artifact and is not shipped in the image.
await writeFile(new URL('chart.umd.min.js', vendor), source.replace(/^\/\/# sourceMappingURL=.*$/m, '').trimEnd() + '\n');
await copyFile(new URL('LICENSE.md', chart), new URL('chart.LICENSE', vendor));
