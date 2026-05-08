/** @type {import('tailwindcss').Config} */
module.exports = {
  content: [
    './app/**/*.{js,jsx,ts,tsx}',
    './components/**/*.{js,jsx,ts,tsx}',
    './src/**/*.{js,jsx,ts,tsx}',
  ],
  presets: [require('nativewind/preset')],
  theme: {
    extend: {
      colors: {
        background: 'hsl(var(--background) / <alpha-value>)',
        foreground: 'hsl(var(--foreground) / <alpha-value>)',
        card: {
          DEFAULT: 'hsl(var(--card) / <alpha-value>)',
          foreground: 'hsl(var(--card-foreground) / <alpha-value>)',
        },
        popover: {
          DEFAULT: 'hsl(var(--popover) / <alpha-value>)',
          foreground: 'hsl(var(--popover-foreground) / <alpha-value>)',
        },
        primary: {
          DEFAULT: 'hsl(var(--primary) / <alpha-value>)',
          foreground: 'hsl(var(--primary-foreground) / <alpha-value>)',
        },
        secondary: {
          DEFAULT: 'hsl(var(--secondary) / <alpha-value>)',
          foreground: 'hsl(var(--secondary-foreground) / <alpha-value>)',
        },
        muted: {
          DEFAULT: 'hsl(var(--muted) / <alpha-value>)',
          foreground: 'hsl(var(--muted-foreground) / <alpha-value>)',
        },
        accent: {
          DEFAULT: 'hsl(var(--accent) / <alpha-value>)',
          foreground: 'hsl(var(--accent-foreground) / <alpha-value>)',
        },
        destructive: {
          DEFAULT: 'hsl(var(--destructive) / <alpha-value>)',
          foreground: 'hsl(var(--destructive-foreground) / <alpha-value>)',
        },
        border: 'hsl(var(--border) / <alpha-value>)',
        input: 'hsl(var(--input) / <alpha-value>)',
        ring: 'hsl(var(--ring) / <alpha-value>)',
        'status-running': {
          DEFAULT: 'hsl(var(--status-running) / <alpha-value>)',
          foreground: 'hsl(var(--status-running-foreground) / <alpha-value>)',
          muted: 'hsl(var(--status-running-muted) / <alpha-value>)',
        },
        'status-warning': {
          DEFAULT: 'hsl(var(--status-warning) / <alpha-value>)',
          foreground: 'hsl(var(--status-warning-foreground) / <alpha-value>)',
          muted: 'hsl(var(--status-warning-muted) / <alpha-value>)',
        },
        'status-success': {
          DEFAULT: 'hsl(var(--status-success) / <alpha-value>)',
          foreground: 'hsl(var(--status-success-foreground) / <alpha-value>)',
          muted: 'hsl(var(--status-success-muted) / <alpha-value>)',
        },
        'status-tool': {
          DEFAULT: 'hsl(var(--status-tool) / <alpha-value>)',
          foreground: 'hsl(var(--status-tool-foreground) / <alpha-value>)',
          muted: 'hsl(var(--status-tool-muted) / <alpha-value>)',
        },
      },
      borderRadius: {
        lg: 'var(--radius)',
        md: 'calc(var(--radius) - 2px)',
        sm: 'calc(var(--radius) - 4px)',
      },
      fontFamily: {
        mono: ['Menlo', 'Monaco', 'Consolas', 'Courier New', 'monospace'],
      },
    },
  },
  plugins: [],
};
