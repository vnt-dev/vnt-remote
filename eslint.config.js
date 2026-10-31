import js from '@eslint/js';
import ts from 'typescript-eslint';
import vue from 'eslint-plugin-vue';

export default [
  { ignores: ['dist/**', 'target/**', 'src-tauri/gen/**', '.zcode/**'] },
  js.configs.recommended,
  ...ts.configs.recommended,
  ...vue.configs['flat/essential'],
  {
    files: ['**/*.vue'],
    languageOptions: { parserOptions: { parser: ts.parser, extraFileExtensions: ['.vue'] } },
    rules: {
      'vue/multi-word-component-names': 'off',
      // TypeScript checks unresolved names, including type-only DOM references.
      'no-undef': 'off',
    },
  },
];
