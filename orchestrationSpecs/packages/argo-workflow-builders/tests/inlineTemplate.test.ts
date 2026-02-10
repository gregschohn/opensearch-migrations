import {INLINE, renderWorkflowTemplate, typeToken, WorkflowBuilder} from "../src";

describe("inline template tests", () => {
    it("should support inline container template in steps", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline" })
            .addTemplate("testInline", t => t
                .addSteps(sb => sb
                    .addStep("inline-container", INLINE, b => b
                        .addContainer((cb: any) => cb
                            .addImageInfo("busybox", "Always")
                            .addCommand(["echo", "hello"])
                            .addResources({})
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        expect(JSON.stringify(rendered)).toContain("inline-container");
        expect(JSON.stringify(rendered)).toContain("\"inline\"");
        expect(JSON.stringify(rendered)).toContain("busybox");
    });

    it("should support inline steps template in steps", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-steps" })
            .addTemplate("testInlineSteps", t => t
                .addSteps(sb => sb
                    .addStep("nested-steps", INLINE, b => b
                        .addSteps((inner: any) => inner
                            .addStep("inner-step", INLINE, (b2: any) => b2
                                .addContainer((cb: any) => cb
                                    .addImageInfo("alpine", "Always")
                                    .addCommand(["ls"])
                                    .addResources({})
                                )
                            )
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        expect(JSON.stringify(rendered)).toContain("nested-steps");
        expect(JSON.stringify(rendered)).toContain("inner-step");
        expect(JSON.stringify(rendered)).toContain("\"inline\"");
    });

    it("should support inline dag template in dag", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-dag" })
            .addTemplate("testInlineDag", t => t
                .addDag(db => db
                    .addTask("inline-task", INLINE, b => b
                        .addContainer((cb: any) => cb
                            .addImageInfo("nginx", "Always")
                            .addCommand(["nginx"])
                            .addResources({})
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        expect(JSON.stringify(rendered)).toContain("inline-task");
        expect(JSON.stringify(rendered)).toContain("\"inline\"");
        expect(JSON.stringify(rendered)).toContain("nginx");
    });

    it("should support inline template with inputs", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-inputs" })
            .addTemplate("testInlineInputs", t => t
                .addSteps(sb => sb
                    .addStep("with-inputs", INLINE, b => b
                        .addRequiredInput("message", typeToken<string>())
                        .addContainer((cb: any) => cb
                            .addImageInfo("busybox", "Always")
                            .addCommand(["echo", cb.inputs.message])
                            .addResources({})
                        ),
                        (ctx: any) => ctx.register({ message: "hello world" })
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        expect(JSON.stringify(rendered)).toContain("with-inputs");
        expect(JSON.stringify(rendered)).toContain("message");
        expect(JSON.stringify(rendered)).toContain("hello world");
    });

    it("should support inline template with outputs", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-outputs" })
            .addTemplate("testInlineOutputs", t => t
                .addSteps(sb => sb
                    .addStep("with-outputs", INLINE, b => b
                        .addContainer((cb: any) => cb
                            .addImageInfo("busybox", "Always")
                            .addCommand(["echo", "result"])
                            .addResources({})
                        )
                        .addExpressionOutput("result", () => "success" as string)
                    )
                    .addStep("use-output", INLINE, b => b
                        .addContainer((cb: any) => cb
                            .addImageInfo("busybox", "Always")
                            .addCommand(["echo", "with-outputs"])
                            .addResources({})
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        expect(JSON.stringify(rendered)).toContain("with-outputs");
        expect(JSON.stringify(rendered)).toContain("use-output");
        expect(JSON.stringify(rendered)).toContain("result");
    });
});
