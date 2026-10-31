import { ref } from 'vue';
import { api } from './api';

export const edition = ref<'installed' | 'portable' | null>(null);
let pending: Promise<void> | undefined;
export function loadEdition(): Promise<void> {
  return (pending ??= api
    .appEdition()
    .then((value) => {
      edition.value = value;
    })
    .catch((error) => {
      pending = undefined;
      throw error;
    }));
}
