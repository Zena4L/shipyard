import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import RunSheet from "./RunSheet.svelte";

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';

function sheet(props: Partial<{ run: unknown; taken: (name: string) => boolean }> = {}) {
  const onrun = vi.fn();
  const onedit = vi.fn();
  const rendered = render(RunSheet, {
    props: { run: null, taken: () => false, onrun, onedit, ...props } as never,
  });
  const field = (id: string) => rendered.container.querySelector<HTMLInputElement>(`#${id}`)!;
  return { ...rendered, onrun, onedit, field };
}

describe("the Run sheet", () => {
  it("says its command as you type, and hands over the spec alone", async () => {
    const { container, field, onrun, getByRole } = sheet();
    const echo = () => container.querySelector("#cliEcho")?.textContent;
    expect(echo()).toBe("shipyard run -d --memory 256m alpine:3.20");
    await fireEvent.input(field("rName"), { target: { value: "box" } });
    await fireEvent.input(field("rPort"), { target: { value: "8081:80" } });
    await fireEvent.change(container.querySelector("#rMem")!, { target: { value: "1024" } });
    expect(echo()).toBe("shipyard run -d --name box --memory 1g -p 8081:80 alpine:3.20");
    await fireEvent.click(getByRole("button", { name: "Run" }));
    expect(onrun).toHaveBeenCalledWith({ image: "alpine:3.20", name: "box", memoryMib: 1024, port: "8081:80" });
  });

  it("refuses a name the yard has, and runs nothing", async () => {
    const { field, onrun, getByRole } = sheet({ taken: (name) => name === "api" });
    await fireEvent.input(field("rName"), { target: { value: "api" } });
    await fireEvent.click(getByRole("button", { name: "Run" }));
    expect(onrun).not.toHaveBeenCalled();
    expect(field("rName").validationMessage).toBe('"api" is already in the yard');
  });

  it("says a pull's progress and a failure as text, and waits while it runs", () => {
    // The image the run was started with, whatever the field says now.
    const going = sheet({ run: { image: "nginx:1", going: true, line: HOSTILE, error: null } });
    expect(going.container.textContent).toContain(`Pulling nginx:1: ${HOSTILE}`);
    expect((going.getByRole("button", { name: "Run" }) as HTMLButtonElement).disabled).toBe(true);
    cleanup();
    const failed = sheet({ run: { image: "nginx:1", going: false, line: null, error: { kind: "api", message: HOSTILE } } });
    expect(failed.container.querySelector(".alert")?.textContent).toBe(HOSTILE);
    expect(failed.container.querySelector("img")).toBeNull();
  });

  it("starts from the image it was opened with", () => {
    const { container } = render(RunSheet, {
      props: { image: "redis:7.4-alpine", run: null, taken: () => false, onrun: vi.fn(), onedit: vi.fn() } as never,
    });
    expect(container.querySelector("#cliEcho")?.textContent).toBe("shipyard run -d --memory 256m redis:7.4-alpine");
  });
});
