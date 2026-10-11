import { nodeResolve } from '@rollup/plugin-node-resolve';

export default {
  input: 'editor.js',
  output: {
    file: '../codemirror.js',
    format: 'iife',
    name: 'CodeMirrorBundle'
  },
  plugins: [nodeResolve()]
};