import { type ClassValue, clsx } from 'clsx';
import { twMerge } from 'tailwind-merge';

/** Merges conditional class names, letting later utilities win. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
