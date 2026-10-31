import { computed, nextTick, onUnmounted, ref, shallowRef, watch, type Ref } from 'vue';

// Keep a preview order while dragging; only persist the devices on release.
export function useDeviceSort<T extends { id: string }>(devices: Ref<T[]>) {
  const grid = ref<HTMLElement>();
  const preview = shallowRef<T[] | null>(null);
  const draggedId = ref('');
  const sortedDevices = computed(() => preview.value ?? devices.value);
  let active: {
    pointerId: number;
    id: string;
    card: HTMLElement;
    rect: DOMRect;
    startX: number;
    startY: number;
    x: number;
    y: number;
    floating?: HTMLElement;
  } | null = null;
  let frame = 0;
  let lastFrame = 0;

  function cleanup() {
    cancelAnimationFrame(frame);
    frame = 0;
    lastFrame = 0;
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', release);
    window.removeEventListener('pointercancel', cancel);
    window.removeEventListener('blur', cancel);
    window.removeEventListener('keydown', keydown, true);
    document.removeEventListener('visibilitychange', cancel);
    document.body.classList.remove('device-sorting');
    active = null;
    draggedId.value = '';
    preview.value = null;
  }

  function cancel() {
    active?.floating?.remove();
    cleanup();
  }

  function keydown(event: KeyboardEvent) {
    if (event.key !== 'Escape') return;
    event.preventDefault();
    event.stopPropagation();
    cancel();
  }

  function release(event: PointerEvent) {
    if (!active || event.pointerId !== active.pointerId) return;
    if (active.floating) {
      cancelAnimationFrame(frame);
      active.x = event.clientX;
      active.y = event.clientY;
      tick(performance.now());
    }
    const floating = active.floating;
    const card = active.card;
    const container = grid.value;
    if (preview.value) devices.value = [...preview.value];
    cleanup();
    // Settle the floating card into its reserved slot before removing it.
    if (floating) {
      void nextTick(() => {
        const bounds = container?.getBoundingClientRect();
        const target = bounds
          ? { left: bounds.left + card.offsetLeft, top: bounds.top + card.offsetTop }
          : card.getBoundingClientRect();
        const from = floating.getBoundingClientRect();
        const animation = floating.animate(
          [
            { transform: `translate3d(${from.left}px, ${from.top}px, 0)` },
            { transform: `translate3d(${target.left}px, ${target.top}px, 0)` },
          ],
          {
            duration: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : 140,
            easing: 'ease-out',
          },
        );
        animation.onfinish = () => floating.remove();
        animation.oncancel = () => floating.remove();
      });
    }
  }

  function tick(time: number) {
    const state = active;
    const container = grid.value;
    if (!state?.floating || !container) return;
    const elapsed = lastFrame ? Math.min(time - lastFrame, 32) : 16;
    lastFrame = time;
    const left = state.rect.left + state.x - state.startX;
    const top = state.rect.top + state.y - state.startY;
    state.floating.style.transform = `translate3d(${left}px, ${top}px, 0)`;

    // Scroll the device pane while hovering near its top or bottom edge.
    const scroll = container.closest<HTMLElement>('.main');
    if (scroll) {
      const bounds = scroll.getBoundingClientRect();
      if (state.x >= bounds.left && state.x <= bounds.right) {
        const speed =
          state.y < bounds.top + 56
            ? -Math.min(1, (bounds.top + 56 - state.y) / 56)
            : state.y > bounds.bottom - 56
              ? Math.min(1, (state.y - bounds.bottom + 56) / 56)
              : 0;
        scroll.scrollTop += speed * elapsed * 0.7;
      }
    }

    const bounds = container.getBoundingClientRect();
    const centerX = left + state.rect.width / 2;
    const centerY = top + state.rect.height / 2;
    const cards = [...container.querySelectorAll<HTMLElement>('[data-device-id]')];
    // offsetLeft/Top describe layout slots, unaffected by the move animation.
    let nearest = -1;
    let distance = Infinity;
    if (
      centerX >= bounds.left - 16 &&
      centerX <= bounds.right + 16 &&
      centerY >= bounds.top - 16 &&
      centerY <= bounds.bottom + 16
    ) {
      cards.forEach((card, index) => {
        const x = bounds.left + card.offsetLeft + card.offsetWidth / 2;
        const y = bounds.top + card.offsetTop + card.offsetHeight / 2;
        const next = (centerX - x) ** 2 + (centerY - y) ** 2;
        if (next < distance) {
          distance = next;
          nearest = index;
        }
      });
    }
    const from = preview.value?.findIndex((device) => device.id === state.id) ?? -1;
    if (preview.value && nearest >= 0 && from >= 0 && nearest !== from) {
      const list = [...preview.value];
      const [device] = list.splice(from, 1);
      list.splice(nearest, 0, device);
      preview.value = list;
    }
    frame = requestAnimationFrame(tick);
  }

  function move(event: PointerEvent) {
    const state = active;
    if (!state || event.pointerId !== state.pointerId) return;
    event.preventDefault();
    state.x = event.clientX;
    state.y = event.clientY;
    if (state.floating || Math.hypot(state.x - state.startX, state.y - state.startY) < 4) return;
    const floating = state.card.cloneNode(true) as HTMLElement;
    floating.classList.add('dev-card-floating');
    floating.setAttribute('aria-hidden', 'true');
    floating.inert = true;
    floating.style.width = `${state.rect.width}px`;
    floating.style.height = `${state.rect.height}px`;
    floating.style.transform = `translate3d(${state.rect.left}px, ${state.rect.top}px, 0)`;
    document.body.append(floating);
    state.floating = floating;
    preview.value = [...devices.value];
    draggedId.value = state.id;
    frame = requestAnimationFrame(tick);
  }

  function start(id: string, event: PointerEvent) {
    if (event.button !== 0 || event.isPrimary === false || active) return;
    const card = (event.currentTarget as HTMLElement).closest<HTMLElement>('[data-device-id]');
    if (!card) return;
    event.preventDefault();
    active = {
      pointerId: event.pointerId,
      id,
      card,
      rect: card.getBoundingClientRect(),
      startX: event.clientX,
      startY: event.clientY,
      x: event.clientX,
      y: event.clientY,
    };
    document.body.classList.add('device-sorting');
    window.addEventListener('pointermove', move, { passive: false });
    window.addEventListener('pointerup', release);
    window.addEventListener('pointercancel', cancel);
    window.addEventListener('blur', cancel);
    window.addEventListener('keydown', keydown, true);
    document.addEventListener('visibilitychange', cancel);
  }

  watch(devices, () => {
    if (active) cancel();
  });
  onUnmounted(cancel);
  return { grid, sortedDevices, draggedId, start, cancel };
}
