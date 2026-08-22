import js from '@eslint/js';
import next from 'eslint-config-next';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  {
    ignores: [
      '.next/**',
      'node_modules/**',
      'coverage/**',
      'playwright-report/**',
      'test-results/**',
      'next-env.d.ts',
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  ...next,
  {
    rules: {
      // The API boundary is typed. `unknown` plus validation is the escape
      // hatch for genuinely unknown shapes, never `any`.
      '@typescript-eslint/no-explicit-any': 'error',
      // Correct `import type` usage is already enforced at compile time by
      // `verbatimModuleSyntax` in tsconfig, so the equivalent lint rule (which
      // would require slow typed linting) is deliberately omitted.
      '@typescript-eslint/no-unused-vars': [
        'error',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_' },
      ],
      eqeqeq: ['error', 'always', { null: 'ignore' }],
      'no-console': ['error', { allow: ['warn', 'error'] }],
    },
  },
  {
    files: ['**/*.test.ts', '**/*.test.tsx', 'test/**/*.ts', 'e2e/**/*.ts'],
    rules: { '@typescript-eslint/no-non-null-assertion': 'off' },
  },
  {
    // End-to-end specs are not React. The test runner's fixture callback is
    // named `use`, which the React hook rules otherwise mistake for `React.use`.
    files: ['e2e/**/*.ts'],
    rules: {
      'react-hooks/rules-of-hooks': 'off',
    },
  },
);
