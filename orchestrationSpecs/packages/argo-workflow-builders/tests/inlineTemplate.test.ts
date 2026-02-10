import {INLINE, renderWorkflowTemplate, typeToken, WorkflowBuilder, TemplateBuilder, ContainerBuilder} from "../src";
import {toSafeYamlOutput} from "../src/utils";

describe("inline template tests", () => {
    it("should support inline container template in steps", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline" })
            .addTemplate("testInline", t => t
                .addSteps(sb => sb
                    .addStep("inline-container", INLINE, (b: TemplateBuilder<any, {}, {}, {}>) => b
                        .addContainer((cb: ContainerBuilder<any, any, any, any, any, any>) => cb
                            .addImageInfo("busybox", "IfNotPresent")
                            .addCommand(["echo", "hello"])
                            .addResources({requests: {cpu: "10m"}})
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        // console.log(toSafeYamlOutput(rendered));
        const template = rendered.spec.templates.find((t: any) => t.name === "testinline");
        expect(template.steps[0][0].name).toBe("inline-container");
        expect(template.steps[0][0].inline).toBeDefined();
        expect(template.steps[0][0].inline.container).toBeDefined();
        expect(template.steps[0][0].inline.container.image).toBe("busybox");
    });

    it("should support inline steps template in steps", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-steps" })
            .addTemplate("testInlineSteps", t => t
                .addSteps(sb => sb
                    .addStep("nested-steps", INLINE, (b: TemplateBuilder<any, {}, {}, {}>) => b
                        .addSteps(inner => inner
                            .addStep("inner-step", INLINE, (b2: TemplateBuilder<any, {}, {}, {}>) => b2
                                .addContainer((cb: ContainerBuilder<any, any, any, any, any, any>) => cb
                                    .addImageInfo("alpine", "IfNotPresent")
                                    .addCommand(["ls"])
                                    .addResources({requests: {cpu: "10m"}})
                                )
                            )
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        const template = rendered.spec.templates.find((t: any) => t.name === "testinlinesteps");
        expect(template.steps[0][0].name).toBe("nested-steps");
        expect(template.steps[0][0].inline).toBeDefined();
        expect(template.steps[0][0].inline.steps).toBeDefined();
    });

    it("should support inline dag template in dag", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-dag" })
            .addTemplate("testInlineDag", t => t
                .addDag(db => db
                    .addTask("inline-task", INLINE, (b: TemplateBuilder<any, {}, {}, {}>) => b
                        .addContainer((cb: ContainerBuilder<any, any, any, any, any, any>) => cb
                            .addImageInfo("nginx", "IfNotPresent")
                            .addCommand(["nginx"])
                            .addResources({requests: {cpu: "10m"}})
                        )
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        const template = rendered.spec.templates.find((t: any) => t.name === "testinlinedag");
        expect(template.dag.tasks[0].name).toBe("inline-task");
        expect(template.dag.tasks[0].inline).toBeDefined();
        expect(template.dag.tasks[0].inline.container).toBeDefined();
    });

    it("should support inline template with inputs", () => {
        const wf = WorkflowBuilder.create({ k8sResourceName: "test-inline-inputs" })
            .addTemplate("testInlineInputs", t => t
                .addSteps(sb => sb
                    .addStep("with-inputs", INLINE, (b: TemplateBuilder<any, {}, {}, {}>) => b
                        .addRequiredInput("message", typeToken<string>())
                        .addContainer((cb: any) => cb
                            .addImageInfo("busybox", "IfNotPresent")
                            .addCommand(["echo", cb.inputs.message])
                            .addResources({requests: {cpu: "10m"}})
                        ),
                        (ctx: any) => ctx.register({ message: "hello world" })
                    )
                )
            )
            .getFullScope();

        const rendered = renderWorkflowTemplate(wf);
        const template = rendered.spec.templates.find((t: any) => t.name === "testinlineinputs");
        expect(template.steps[0][0].inline).toBeDefined();
        expect(template.steps[0][0].arguments.parameters[0].name).toBe("message");
        expect(template.steps[0][0].arguments.parameters[0].value).toBe("hello world");
    });
});
