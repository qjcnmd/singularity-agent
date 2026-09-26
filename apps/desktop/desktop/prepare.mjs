import { execFileSync } from 'node:child_process'
import { mkdirSync, copyFileSync } from 'node:fs'
import { resolve, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const web = resolve(fileURLToPath(new URL('..', import.meta.url)))
const root = resolve(web, '../..')
const profile = process.argv[2] ?? 'debug'
if (!['debug', 'release'].includes(profile)) throw new Error('Expected debug or release')
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--no-deps', '--format-version', '1'], { cwd: root, encoding: 'utf8' }))
const destination = join(web, 'desktop-runtime')
mkdirSync(destination, { recursive: true })
copyFileSync(join(metadata.target_directory, profile, 'singularity.exe'), join(destination, 'singularity.exe'))
console.log(`Prepared ${profile} Rust AppServer`)
